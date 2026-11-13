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

#[path = "catalog_deparse_operators.rs"]
mod operators;
#[path = "catalog_query_deparse.rs"]
mod query;
#[path = "catalog_deparse_types.rs"]
mod types;

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
    /// Scratch space for typing expressions the deparser does not model.
    pub arena: &'s Arena,
    /// The catalog identities a stored query captured at CREATE, which its
    /// nested queries' names are bound to.
    pub dependencies: Option<crate::storage::StoredQueryDependencyView<'s>>,
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
    /// The base type OID of the domain whose constraint is printed, which
    /// `VALUE` has.
    pub domain_value: Option<i32>,
}

/// A FROM item: the name stored references qualify columns with, the name
/// printed for it (a renamed table's current name unless it has an alias)
/// and its columns. A query's scope lists its own items (level 0) before
/// those of the queries around it (one level per enclosing query).
#[derive(Clone, Copy)]
pub(crate) struct Joined<'s> {
    pub name: &'s str,
    pub printed: &'s str,
    pub columns: Columns<'s>,
    /// Whether an unqualified column can name this item; a rule's `OLD` and
    /// `NEW` are reached only by their names.
    pub unqualified: bool,
    /// Whether a qualified reference can name this item. The members of an
    /// aliased join keep their names reserved but cannot be named.
    pub addressable: bool,
    /// Query levels out from the expression's own.
    pub level: u8,
    /// The row type a whole-row reference to a derived item has.
    pub row_type: Option<i32>,
    /// The items of the same level, by position within it, whose columns
    /// this join's merged `USING` columns hide from unqualified references.
    pub hides: u64,
    /// A `WITH` query a FROM item of this level or an inner one can name.
    pub cte: bool,
}

/// The columns of a FROM item.
#[derive(Clone, Copy)]
pub(crate) enum Columns<'s> {
    Table(&'s TableDef),
    Derived(&'s [Column<'s>]),
}

/// A column of a derived FROM item: a subquery, function, `VALUES` list,
/// `WITH` query, view or join.
#[derive(Clone, Copy)]
pub(crate) struct Column<'s> {
    pub name: &'s str,
    pub type_oid: Option<i32>,
    /// A join's merged `USING` column, printed as the input column it reads
    /// (`None` qualifier: a `FULL` join's, printed unqualified).
    pub merged: Option<(Option<&'s str>, &'s str)>,
}

impl<'s> Columns<'s> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Table(table) => table.columns().len(),
            Self::Derived(columns) => columns.len(),
        }
    }

    pub(crate) fn name(&self, index: usize) -> &'s str {
        match self {
            Self::Table(table) => table.columns()[index].name.as_str(),
            Self::Derived(columns) => columns[index].name,
        }
    }

    pub(crate) fn position(&self, name: &str) -> Option<usize> {
        (0..self.len()).find(|&index| self.name(index) == name)
    }

    fn merged(&self, index: usize) -> Option<(Option<&'s str>, &'s str)> {
        match self {
            Self::Table(_) => None,
            Self::Derived(columns) => columns[index].merged,
        }
    }
}

impl<'s> Joined<'s> {
    pub(crate) fn new(name: &'s str, table: &'s TableDef) -> Self {
        Self {
            name,
            printed: name,
            columns: Columns::Table(table),
            unqualified: true,
            addressable: true,
            level: 0,
            row_type: None,
            hides: 0,
            cte: false,
        }
    }

    /// A rule's `OLD` or `NEW` row in a rule action.
    pub(crate) fn rule_row(name: &'s str, table: &'s TableDef) -> Self {
        Self {
            unqualified: false,
            ..Self::new(name, table)
        }
    }

    /// A rule's `OLD` or `NEW` row of a relation with these columns.
    pub(crate) fn rule_columns(name: &'s str, columns: Columns<'s>) -> Self {
        Self {
            name,
            printed: name,
            columns,
            unqualified: false,
            addressable: true,
            level: 0,
            row_type: None,
            hides: 0,
            cte: false,
        }
    }

    /// The table definition of a physical item.
    pub(crate) fn table(&self) -> Option<&'s TableDef> {
        match self.columns {
            Columns::Table(table) => Some(table),
            Columns::Derived(_) => None,
        }
    }
}

/// A whole-row reference's item.
#[derive(Clone, Copy)]
struct RowSource<'j> {
    printed: &'j str,
    columns: Columns<'j>,
    row_type: Option<i32>,
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
    /// The operand of a prefix operator.
    Prefix,
    /// An argument `get_func_sql_syntax` prints in SQL syntax.
    SqlSyntax,
    /// The operand of `COLLATE`.
    Collate,
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
    let mut output = Output::new(arena);
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
    /// `indentLevel` of PostgreSQL's indented query layout.
    indent: i32,
    /// The target-list wrap column; negative never wraps.
    wrap: i32,
    /// Printing a query in its indented layout, where a `CASE` starts its
    /// own lines.
    layout: bool,
}

impl<'a> Output<'a> {
    fn new(arena: &'a Arena) -> Self {
        Self {
            arena,
            text: ArenaString::new(arena),
            supported: true,
            indent: 0,
            wrap: 0,
            layout: false,
        }
    }
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

/// `select_common_type` of two typed inputs: the numeric category's
/// preferred direction, and `text` over the other string types.
fn common_type(current: i32, next: i32) -> i32 {
    let rank = |type_oid: i32| match type_oid {
        oid::INT2 => Some(1),
        oid::INT4 => Some(2),
        oid::INT8 => Some(3),
        oid::NUMERIC => Some(4),
        oid::FLOAT4 => Some(5),
        oid::FLOAT8 => Some(6),
        _ => None,
    };
    match (rank(current), rank(next)) {
        (Some(have), Some(found)) if found > have => next,
        _ if next == oid::TEXT && matches!(current, oid::VARCHAR | oid::BPCHAR) => next,
        _ => current,
    }
}

/// The array type whose elements have `element`'s type.
fn array_of(element: i32) -> Option<i32> {
    crate::sql::types::ArrElem::from_coltype(ColType::from_oid(element)?)?.try_array_oid()
}

/// The element type of an array type.
fn element_of(array: i32) -> Option<i32> {
    match ColType::from_oid(array)? {
        ColType::Array(element) => Some(element.element_oid()),
        _ => None,
    }
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
            && name == "value"
            && let Some(type_oid) = self.domain_value
        {
            return Ok(Some(type_oid));
        }
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
        if let Some((source, column)) = self.joined_source(qualifier, name) {
            return match source.columns {
                Columns::Table(table) => super::catalog_column_type_oid(
                    self.storage,
                    &table.columns()[column],
                    self.txid,
                )
                .map(Some),
                Columns::Derived(columns) => Ok(columns[column].type_oid),
            };
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

    /// Whether a column reference names a parameter, a FROM item's column or
    /// a column of the context's table.
    fn column_known(&self, qualifier: Option<&str>, name: &str) -> bool {
        (qualifier.is_none()
            && ((name == "value" && self.domain_value.is_some())
                || self
                    .parameters
                    .iter()
                    .any(|parameter| parameter.name.as_str() == name)))
            || self.joined_source(qualifier, name).is_some()
            || self.table.is_some_and(|table| {
                qualifier.is_none_or(|qualifier| qualifier == table.name.as_str())
                    && table.column_index(name).is_some()
            })
    }

    /// The one FROM item a column reference names, searching the innermost
    /// query level first, and the column's position in it.
    fn joined_source(&self, qualifier: Option<&str>, name: &str) -> Option<(&Joined<'_>, usize)> {
        let deepest = self.joined.iter().map(|joined| joined.level).max()?;
        for level in 0..=deepest {
            let mut found = None;
            let mut first = None;
            for (index, joined) in self.joined.iter().enumerate() {
                if joined.level != level || joined.cte {
                    continue;
                }
                let start = *first.get_or_insert(index);
                let reachable = match qualifier {
                    Some(qualifier) => joined.addressable && joined.name == qualifier,
                    None => joined.unqualified,
                };
                let Some(column) = reachable.then(|| joined.columns.position(name)).flatten()
                else {
                    continue;
                };
                // A join's merged column hides its inputs' columns of the
                // same name from unqualified references.
                let hidden = qualifier.is_none()
                    && self.joined[start..].iter().any(|other| {
                        other.level == level
                            && index - start < 64
                            && other.hides & (1 << (index - start)) != 0
                            && other.columns.position(name).is_some()
                    });
                if hidden {
                    continue;
                }
                if found.is_some() {
                    return None;
                }
                found = Some((joined, column));
            }
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// The relation a whole-row reference names, with the name it is exposed
    /// under: `t.*`, or a bare `t` that is neither a column nor a parameter.
    fn whole_row(&self, expression: &Expr<'_>) -> Option<RowSource<'_>> {
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
                .then(|| RowSource {
                    printed: table.name.as_str(),
                    columns: Columns::Table(table),
                    row_type: self.row_type(table),
                });
        }
        if bare && self.joined_source(None, name).is_some() {
            return None;
        }
        let deepest = self.joined.iter().map(|joined| joined.level).max()?;
        (0..=deepest).find_map(|level| {
            self.joined
                .iter()
                .find(|joined| {
                    joined.level == level
                        && !joined.cte
                        && joined.addressable
                        && joined.name == name
                })
                .map(|joined| RowSource {
                    printed: joined.printed,
                    columns: joined.columns,
                    row_type: joined
                        .row_type
                        .or_else(|| joined.table().and_then(|table| self.row_type(table))),
                })
        })
    }

    fn row_type(&self, table: &TableDef) -> Option<i32> {
        self.storage
            .find_visible(table.schema.as_str(), table.name.as_str(), self.txid)
            .map(|slot| super::table_row_type_oid(self.storage, slot, false))
    }

    /// Whether columns print qualified: a query of several FROM items, a
    /// nested query or a rule's query, whose range table also holds `OLD`
    /// and `NEW`.
    fn qualifies(&self) -> bool {
        self.qualify || self.joined.iter().filter(|joined| !joined.cte).count() > 1
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
        if matches!(expression, Expr::Str(_) | Expr::Null) {
            return Ok(None);
        }
        Ok(match self.modeled_type(expression)? {
            Some(found) => Some(found),
            None => self.inferred_type(expression),
        })
    }

    /// The types this module derives itself, as PostgreSQL's parser does.
    fn modeled_type(&self, expression: &Expr<'_>) -> Result<Option<i32>, SqlError> {
        Ok(match expression {
            Expr::Int(value) => Some(if i32::try_from(*value).is_ok() {
                oid::INT4
            } else {
                oid::INT8
            }),
            Expr::Float(_) | Expr::NumericLit(_) => Some(oid::NUMERIC),
            Expr::Bool(_)
            | Expr::Exists(_)
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::IsNull { .. }
            | Expr::InList { .. }
            | Expr::AnyAll { .. }
            | Expr::Like { .. }
            | Expr::Match { .. } => Some(oid::BOOL),
            Expr::Str(_) | Expr::Null => None,
            _ if self.whole_row(expression).is_some() => {
                self.whole_row(expression).and_then(|row| row.row_type)
            }
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
                    let mut found: Option<i32> = None;
                    for argument in *args {
                        if let Some(argument) = self.type_of(argument)? {
                            found = Some(match found {
                                None => argument,
                                Some(current) => common_type(current, argument),
                            });
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
            Expr::Array(items) => {
                let mut found = None;
                for item in *items {
                    if let Some(element) = self.type_of(item)? {
                        found = array_of(element);
                        break;
                    }
                }
                found
            }
            Expr::Call { args, .. } => match self.selected_builtin(expression)? {
                Some(routine) => {
                    match crate::storage::PolymorphicType::from_oid(routine.result_oid) {
                        // A polymorphic result takes its element type from the
                        // polymorphic arguments, as `enforce_generic_type_consistency`
                        // resolves it.
                        Some(
                            result @ (crate::storage::PolymorphicType::Element
                            | crate::storage::PolymorphicType::Array),
                        ) => {
                            let mut element = None;
                            for (argument, parameter) in args.iter().zip(routine.arguments) {
                                let found =
                                    match crate::storage::PolymorphicType::from_oid(*parameter) {
                                        Some(crate::storage::PolymorphicType::Element) => {
                                            self.type_of(argument)?
                                        }
                                        Some(crate::storage::PolymorphicType::Array) => {
                                            self.type_of(argument)?.and_then(element_of)
                                        }
                                        _ => None,
                                    };
                                if found.is_some() {
                                    element = found;
                                    break;
                                }
                            }
                            match result {
                                crate::storage::PolymorphicType::Element => element,
                                _ => element.and_then(array_of),
                            }
                        }
                        Some(_) => None,
                        None => Some(routine.result_oid),
                    }
                }
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
        // A domain operand is coerced to its base type, as a relabeling the
        // deparser prints as a cast.
        let seen = |type_oid: i32| {
            operator_type(super::domain_root_type_oid(
                self.storage,
                self.txid,
                type_oid,
            ))
        };
        Ok(match (left.map(seen), right.map(seen)) {
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
            variadic,
            star: false,
            ..
        } = expression
        else {
            return Ok(None);
        };
        if args.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
            return Ok(None);
        }
        let mut argument_oids = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (target, argument) in argument_oids.iter_mut().zip(*args) {
            *target = self.type_of(argument)?.unwrap_or(oid::UNKNOWN);
        }
        Ok(
            match self.storage.select_routine_call(
                name,
                argument_names,
                &argument_oids[..args.len()],
                *variadic,
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
        self.type_name_mod(type_oid, -1, output)
    }

    /// `format_type_with_typemod`: a modifier PostgreSQL's grammar would
    /// otherwise supply (`bit`, `bpchar`) is spelled as the quoted name.
    fn type_name_mod(
        &self,
        type_oid: i32,
        type_mod: i32,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        if type_mod < 0 {
            match type_oid {
                oid::BPCHAR => return output.push("bpchar"),
                oid::BIT => return output.push("\"bit\""),
                _ => {}
            }
        }
        if crate::storage::NamedTypeKind::from_oid(type_oid).is_none()
            && let Some(name) =
                crate::sql::eval::funcs::system::format_builtin_type(type_oid, type_mod, true)
        {
            return output.push(name.as_str());
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
        // `ARRAY[...]` coerces its items, not the array.
        if let (Expr::Array(items), Some(member)) = (expression, expected.and_then(element_of)) {
            return self.array(items, Some(member), output);
        }
        let natural = self.type_of(expression)?;
        match (natural, expected) {
            // A row is a `record` without a coercion.
            (Some(natural), Some(expected)) if natural != expected && expected != oid::RECORD => {
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
            Expr::NumericLit(text) => {
                let canonical = self.numeric_text(text, false, output)?;
                self.constant(canonical, oid::NUMERIC, output)
            }
            Expr::Bool(value) => output.push(if *value { "true" } else { "false" }),
            Expr::BitLit(written) => {
                let bits = crate::sql::eval::parse_bits_text(written, output.arena)?;
                self.constant(bits, oid::BIT, output)
            }
            Expr::SchemaColumn { table, name, .. } => self.bare(
                &Expr::Column {
                    qualifier: Some(table),
                    name,
                },
                output,
            ),
            Expr::BoundLiteral { source, .. } => self.bare(source, output),
            Expr::BoundDefault { expression, .. } => self.bare(expression, output),
            Expr::OuterAggregate { call, .. } => self.bare(call, output),
            // `get_variable` prints a whole-row variable as `t.*`.
            _ if self.whole_row(expression).is_some() => {
                let exposed = self.whole_row(expression).map_or("", |row| row.printed);
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
            Expr::Field { base, field } if *field != "*" => self.field(base, field, output),
            Expr::Subscript { base, index } => self.subscript(base, Some(index), None, output),
            Expr::Slice { base, lower, upper } => {
                self.subscript(base, *lower, Some(*upper), output)
            }
            Expr::Collate { operand, collation } => self.collate(operand, collation, output),
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
                    let count = expansion.map_or(0, |row| row.columns.len());
                    for column in 0..count {
                        if !first {
                            output.push(", ")?;
                        }
                        first = false;
                        let row = expansion.expect("counted expansion");
                        let mut identifier = crate::util::StackStr::<260>::new();
                        write_identifier(&mut identifier, row.columns.name(column));
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
                if !self.column_known(*qualifier, name) {
                    return output.unsupported();
                }
                if qualifier.is_none() && *name == "value" && self.domain_value.is_some() {
                    return output.push("VALUE");
                }
                let mut identifier = crate::util::StackStr::<260>::new();
                let source = self.joined_source(*qualifier, name);
                match source.and_then(|(source, column)| source.columns.merged(column)) {
                    // A join's merged column prints as the input it reads.
                    Some((qualifier, name)) => {
                        if let Some(qualifier) = qualifier {
                            write_identifier(&mut identifier, qualifier);
                            let _ = identifier.write_char('.');
                        }
                        write_identifier(&mut identifier, name);
                    }
                    None => {
                        if self.qualifies() {
                            let Some((source, _)) = source else {
                                return output.unsupported();
                            };
                            if self.bare != Some(source.printed) {
                                write_identifier(&mut identifier, source.printed);
                                let _ = identifier.write_char('.');
                            }
                        }
                        write_identifier(&mut identifier, name);
                    }
                }
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
                    let canonical = self.numeric_text(text, true, output)?;
                    self.constant(canonical, oid::NUMERIC, output)
                }
                _ => self.prefix(UnaryOp::Neg, operand, output),
            },
            // `NOT SIMILAR TO` and a row `IS DISTINCT FROM` are one construct.
            Expr::Unary {
                operator: UnaryOp::Not,
                operand:
                    Expr::Call {
                        kind: CallKind::SqlSyntax,
                        name,
                        args,
                        ..
                    },
                source_position,
            } if source_position.is_expanded()
                && matches!(
                    *name,
                    crate::sql::parser::SIMILAR_TO | crate::sql::parser::ROWS_NOT_DISTINCT
                ) =>
            {
                if *name == crate::sql::parser::SIMILAR_TO {
                    self.similar(args, true, output)
                } else {
                    match (row_items(args[0]), row_items(args[1])) {
                        (Some(left), Some(right)) => self.rows_distinct(left, right, false, output),
                        _ => output.unsupported(),
                    }
                }
            }
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
                self.connective(expression, *operator, true, true, output)?;
                self.close(output)
            }
            Expr::Unary {
                operator, operand, ..
            } => self.prefix(*operator, operand, output),
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } => match (row_items(left), row_items(right)) {
                (Some(left), Some(right)) => self.row_comparison(*operator, left, right, output),
                _ => self.binary(*operator, left, right, output),
            },
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
            } => match row_items(operand) {
                // A row's `IN` list is the rows' comparisons, joined by `OR`
                // (`AND` for `NOT IN`).
                Some(fields) => {
                    if list.len() > 1 {
                        self.open(output)?;
                    }
                    for (index, item) in list.iter().enumerate() {
                        if index != 0 {
                            output.push(if *negated { " AND " } else { " OR " })?;
                        }
                        let Some(items) = row_items(item) else {
                            return output.unsupported();
                        };
                        let operator = if *negated {
                            BinaryOp::NotEq
                        } else {
                            BinaryOp::Eq
                        };
                        let pretty_paren = self.pretty && list.len() > 1 && fields.len() > 1;
                        if pretty_paren
                            && (*negated || operator == BinaryOp::NotEq)
                                == (operator == BinaryOp::Eq)
                        {
                            output.push("(")?;
                        }
                        self.row_comparison(operator, fields, items, output)?;
                        if pretty_paren
                            && (*negated || operator == BinaryOp::NotEq)
                                == (operator == BinaryOp::Eq)
                        {
                            output.push(")")?;
                        }
                    }
                    if list.len() > 1 {
                        self.close(output)?;
                    }
                    Ok(())
                }
                None => self.in_list(operand, list, *negated, output),
            },
            Expr::Array(items) => self.array(items, None, output),
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
                let array_type = match (array, self.type_of(array)?) {
                    // A constructor's items take the comparison's common type.
                    (Expr::Array(_), found) => {
                        let member = found.and_then(element_of);
                        let common = match (element, member) {
                            (Some(element), Some(member))
                                if numeric_rank(member) > numeric_rank(element) =>
                            {
                                Some(member)
                            }
                            (Some(element), _) => Some(element),
                            (None, member) => member,
                        };
                        common.and_then(array_of)
                    }
                    (_, Some(found)) => Some(found),
                    // An untyped array literal takes the operand's array type.
                    (_, None) => element.and_then(array_of),
                };
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
                self.write(array, array_type, output)?;
                output.push(")")?;
                self.close(output)
            }
            Expr::Like {
                operand,
                pattern,
                negated,
                case_insensitive,
                escape,
            } => {
                let symbol = match (*negated, *case_insensitive) {
                    (false, false) => "~~",
                    (false, true) => "~~*",
                    (true, false) => "!~~",
                    (true, true) => "!~~*",
                };
                match escape {
                    Some(escape) => self.like_escape(operand, pattern, escape, symbol, output),
                    None => self.operator_pair(operand, pattern, symbol, output),
                }
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
                operand,
                type_name,
                type_mod,
                ..
            } => {
                let Some(target) = self.named_type(type_name) else {
                    return output.unsupported();
                };
                // `transformArrayExpr`: a cast of an ARRAY constructor
                // coerces each element to the element type instead; an empty
                // one keeps the cast as its type.
                if let Expr::Array(items) = operand
                    && !items.is_empty()
                    && *type_mod < 0
                    && let Some(element) = crate::sql::assignment_path::element_type_oid(target)
                    && !items.iter().any(|item| matches!(item, Expr::Array(_)))
                {
                    output.push("ARRAY[")?;
                    for (index, item) in items.iter().enumerate() {
                        if index != 0 {
                            output.push(", ")?;
                        }
                        match self.type_of(item)? {
                            Some(natural) if natural != element => {
                                self.wrapped(item, None, Parent::Cast, true, output)?;
                                output.push("::")?;
                                self.type_name(element, output)?;
                            }
                            _ => self.write(item, Some(element), output)?,
                        }
                    }
                    return output.push("]");
                }
                match operand {
                    // A cast literal is converted while parsing.
                    Expr::Str(_) | Expr::Null => {
                        self.literal_mod(operand, target, *type_mod, output)
                    }
                    // `ARRAY[]` takes the cast's type as its own.
                    Expr::Array([]) => {
                        output.push("ARRAY[]::")?;
                        self.type_name_mod(target, *type_mod, output)
                    }
                    // A constant of the target type is the constant itself.
                    Expr::Int(_)
                        if target == oid::INT4 && self.type_of(operand)? == Some(target) =>
                    {
                        self.bare(operand, output)
                    }
                    _ => {
                        self.wrapped(operand, Some(None), Parent::Cast, true, output)?;
                        output.push("::")?;
                        self.type_name_mod(target, *type_mod, output)
                    }
                }
            }
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name: crate::sql::parser::SIMILAR_TO,
                args,
                ..
            } => self.similar(args, false, output),
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name: crate::sql::parser::ROWS_NOT_DISTINCT,
                args: [left, right],
                ..
            } => match (row_items(left), row_items(right)) {
                (Some(left), Some(right)) => self.rows_distinct(left, right, true, output),
                _ => output.unsupported(),
            },
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name,
                args: [argument],
                ..
            } if name.eq_ignore_ascii_case("grouping") => {
                output.push("GROUPING(")?;
                self.write(argument, None, output)?;
                output.push(")")
            }
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name,
                args,
                ..
            } if name.eq_ignore_ascii_case("grouping") => {
                output.push("GROUPING(")?;
                for (index, argument) in args.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    self.write(argument, None, output)?;
                }
                output.push(")")
            }
            // An operator the grammar could not map to a node of its own.
            Expr::Call {
                kind: CallKind::SqlSyntax,
                name,
                args,
                ..
            } if crate::sql::ast::catalog_operator_call(name).is_some() => {
                let (schema, symbol) =
                    crate::sql::ast::catalog_operator_call(name).expect("checked operator");
                if schema.is_some_and(|schema| schema != "pg_catalog") {
                    return output.unsupported();
                }
                match (args, BinaryOp::from_operator_name(symbol)) {
                    ([left, right], Some(operator)) => self.binary(operator, left, right, output),
                    ([operand], _) => {
                        self.open(output)?;
                        output.push(symbol)?;
                        output.push(" ")?;
                        self.wrapped(operand, None, Parent::Prefix, false, output)?;
                        self.close(output)
                    }
                    ([left, right], None) => {
                        self.open(output)?;
                        let parent = Parent::Operator("operator");
                        self.wrapped(left, None, parent, true, output)?;
                        output.push(" ")?;
                        output.push(symbol)?;
                        output.push(" ")?;
                        self.wrapped(right, None, parent, false, output)?;
                        self.close(output)
                    }
                    _ => output.unsupported(),
                }
            }
            // Both write nothing unless the call is theirs.
            Expr::Call { .. } if self.sql_syntax_call(expression, output)? => Ok(()),
            Expr::Call { .. } if self.ordered_set_aggregate(expression, output)? => Ok(()),
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
                order_by: [],
                filter,
                over,
                ..
            } => {
                output.push("count(*)")?;
                self.call_suffix(*filter, *over, output)
            }
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
                self.call_prefix(expression, output)?;
                for (index, argument) in args.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    self.write(argument, Some(parameter_oids[index]), output)?;
                }
                self.call_close(expression, output)
            }
            Expr::Call {
                name,
                args,
                argument_names,
                variadic,
                ..
            } => {
                let Some(routine) = self.selected_builtin(expression)? else {
                    return output.unsupported();
                };
                if !routine.schema.eq("pg_catalog") {
                    return output.unsupported();
                }
                let mut identifier = crate::util::StackStr::<130>::new();
                write_identifier(&mut identifier, unqualified(name));
                output.push(identifier.as_str())?;
                output.push("(")?;
                self.call_prefix(expression, output)?;
                let last = routine.arguments.len().saturating_sub(1);
                for (index, argument) in args.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    let named = argument_names.get(index).copied().flatten();
                    let parameter = match named {
                        Some(named) => routine
                            .argument_names
                            .iter()
                            .position(|candidate| *candidate == named)
                            .and_then(|position| routine.arguments.get(position).copied()),
                        // Arguments past the variadic parameter fill its
                        // element type, unless the call passes the array.
                        None if index >= last && !*variadic => routine
                            .variadic_element
                            .or_else(|| routine.arguments.get(index).copied()),
                        None => routine.arguments.get(index).copied(),
                    };
                    let Some(parameter) = parameter else {
                        return output.unsupported();
                    };
                    if let Some(named) = named {
                        push_identifier(output, named)?;
                        output.push(" => ")?;
                    }
                    if *variadic && index + 1 == args.len() {
                        output.push("VARIADIC ")?;
                    }
                    // An `"any"` parameter keeps an untyped literal unknown;
                    // polymorphic parameters take the argument as it is.
                    if parameter == oid::ANY
                        && let Expr::Str(text) = argument
                    {
                        quote_literal(output, text)?;
                        continue;
                    }
                    let expected = (crate::storage::PolymorphicType::from_oid(parameter).is_none()
                        && parameter != oid::ANY)
                        .then_some(parameter);
                    self.write(argument, expected, output)?;
                }
                self.call_close(expression, output)
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
                origin: crate::sql::ast::CaseOrigin::UnknownTest { .. },
                ..
            } => {
                let Some((operand, negated)) = expression.unknown_test_operand() else {
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
                output.push(if negated {
                    " IS NOT UNKNOWN"
                } else {
                    " IS UNKNOWN"
                })?;
                self.close(output)
            }
            Expr::Case {
                operand,
                whens,
                otherwise,
                origin: crate::sql::ast::CaseOrigin::Written,
            } => {
                if whens.is_empty() {
                    return output.unsupported();
                }
                let mut result = self.type_of(expression)?;
                if result.is_none() {
                    for (_, value) in *whens {
                        if let Some(found) = self.type_of(value)? {
                            result = Some(found);
                            break;
                        }
                    }
                }
                let result = result.or(Some(oid::TEXT));
                // A simple `CASE` compares each `WHEN` value to its operand.
                let when = |output: &mut Output<'_>, condition: &Expr<'_>| match operand {
                    Some(operand) => {
                        let (_, expected) =
                            self.binary_operands(BinaryOp::Eq, operand, condition)?;
                        self.write(condition, expected, output)
                    }
                    None => self.write(condition, Some(oid::BOOL), output),
                };
                let head = |output: &mut Output<'_>| -> Result<(), SqlError> {
                    if let Some(operand) = operand {
                        output.push(" ")?;
                        self.write(operand, None, output)?;
                    }
                    Ok(())
                };
                if output.layout {
                    // The indented layout starts each arm on its own line
                    // and always prints the `ELSE` the parser supplies.
                    output.keyword("CASE", 0, 4, 0)?;
                    head(output)?;
                    for (condition, value) in *whens {
                        output.keyword("WHEN ", 0, 0, 0)?;
                        when(output, condition)?;
                        output.push(" THEN ")?;
                        self.write(value, result, output)?;
                    }
                    output.keyword("ELSE ", 0, 0, 0)?;
                    match otherwise {
                        Some(otherwise) => self.write(otherwise, result, output)?,
                        None => self.literal(&Expr::Null, result.unwrap_or(oid::TEXT), output)?,
                    }
                    return output.keyword("END", -4, 0, 0);
                }
                output.push("CASE")?;
                head(output)?;
                for (condition, value) in *whens {
                    output.push(" WHEN ")?;
                    when(output, condition)?;
                    output.push(" THEN ")?;
                    self.write(value, result, output)?;
                }
                if let Some(otherwise) = otherwise {
                    output.push(" ELSE ")?;
                    self.write(otherwise, result, output)?;
                }
                output.push(" END")
            }
            // `get_sublink_expr`: a sublink parenthesizes itself.
            Expr::Exists(select) => {
                output.push("(EXISTS (")?;
                query::sublink(self, select, output)?;
                output.push("))")
            }
            Expr::Subquery(select) => {
                output.push("(")?;
                query::sublink(self, select, output)?;
                output.push(")")
            }
            Expr::ArraySubquery(select) => {
                output.push("ARRAY(")?;
                query::sublink(self, select, output)?;
                output.push(")")
            }
            Expr::InSubquery {
                operand,
                select,
                negated,
            } => {
                // `NOT IN` is the negation of `= ANY`.
                if *negated {
                    self.open(output)?;
                    output.push("NOT ")?;
                }
                output.push("(")?;
                self.write(operand, None, output)?;
                output.push(" IN (")?;
                query::sublink(self, select, output)?;
                output.push("))")?;
                if *negated {
                    self.close(output)?;
                }
                Ok(())
            }
            Expr::QuantifiedSubquery {
                operand,
                operator,
                select,
                all,
            } => {
                let Some(symbol) = operator_symbol(*operator) else {
                    return output.unsupported();
                };
                output.push("(")?;
                self.write(operand, None, output)?;
                output.push(" ")?;
                output.push(symbol)?;
                output.push(if *all { " ALL (" } else { " ANY (" })?;
                query::sublink(self, select, output)?;
                output.push("))")
            }
            _ => output.unsupported(),
        }
    }

    /// `ARRAY[...]`: its items coerced to the element type, `member` when
    /// the consumer gives one, else the items' common type.
    fn array(
        &self,
        items: &[&Expr<'_>],
        member: Option<i32>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let element = match member {
            Some(member) => Some(member),
            None => {
                let mut common: Option<i32> = None;
                for item in items {
                    let found = match item {
                        Expr::Array(_) => self.type_of(item)?.and_then(element_of),
                        _ => self.type_of(item)?,
                    };
                    let Some(found) = found else {
                        continue;
                    };
                    common = Some(match common {
                        Some(current) if numeric_rank(found) <= numeric_rank(current) => current,
                        _ => found,
                    });
                }
                Some(common.unwrap_or(oid::TEXT))
            }
        };
        output.push("ARRAY[")?;
        for (index, item) in items.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            match item {
                Expr::Array(nested) => self.array(nested, element, output)?,
                _ => self.write(item, element, output)?,
            }
        }
        output.push("]")
    }

    /// A numeric literal's canonical text (`numeric_out`), negated when the
    /// parser folded a minus sign into it.
    fn numeric_text<'o>(
        &self,
        text: &str,
        negate: bool,
        output: &Output<'o>,
    ) -> Result<&'o str, SqlError> {
        let written = if negate {
            output
                .arena
                .alloc_str_display(format_args!("-{text}"))
                .map_err(|_| arena_full())?
        } else {
            output.arena.alloc_str(text).map_err(|_| arena_full())?
        };
        match crate::sql::eval::cast_to(
            crate::sql::types::Datum::Text(written),
            ColType::Numeric,
            output.arena,
        ) {
            Ok(value) => output
                .arena
                .alloc_str_display(value)
                .map_err(|_| arena_full()),
            Err(error) => Err(error),
        }
    }

    /// A literal converted to `target` under a type modifier.
    fn literal_mod(
        &self,
        expression: &Expr<'_>,
        target: i32,
        type_mod: i32,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        if type_mod < 0 {
            return self.literal(expression, target, output);
        }
        let mut piece = output.piece();
        self.literal(expression, target, &mut piece)?;
        // The literal ends with its unmodified type name; respell it.
        let text = piece.text.as_str();
        let mut plain = Output::new(output.arena);
        self.type_name(target, &mut plain)?;
        let Some(value) = text.strip_suffix(plain.text.as_str()) else {
            return output.unsupported();
        };
        if !value.ends_with("::") {
            output.push(text)?;
            return output.unsupported();
        }
        output.push(value)?;
        self.type_name_mod(target, type_mod, output)
    }

    /// Whether `expression` is a constant the parser folds (`Const`).
    fn is_constant(&self, expression: &Expr<'_>) -> bool {
        match expression {
            Expr::Int(_)
            | Expr::NumericLit(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::BitLit(_) => true,
            Expr::Unary {
                operator: UnaryOp::Neg,
                operand: Expr::Int(_) | Expr::NumericLit(_),
                ..
            } => true,
            Expr::Cast { operand, .. } => matches!(operand, Expr::Str(_) | Expr::Null),
            _ => false,
        }
    }

    /// `get_const_expr` with `showtype` 1: a constant with its type always
    /// shown.
    fn labeled_constant(
        &self,
        expression: &Expr<'_>,
        expected: Option<i32>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let start = output.text.as_str().len();
        self.write(expression, expected, output)?;
        let text = &output.text.as_str()[start..];
        let labeled = text.starts_with('\'') || text.starts_with("NULL::");
        if !labeled {
            let type_oid = expected.or(self.type_of(expression)?).unwrap_or(oid::TEXT);
            output.push("::")?;
            self.type_name(type_oid, output)?;
        }
        Ok(())
    }

    /// An aggregate's `DISTINCT`, after its opening parenthesis.
    fn call_prefix(&self, expression: &Expr<'_>, output: &mut Output<'_>) -> Result<(), SqlError> {
        if let Expr::Call { distinct: true, .. } = expression {
            output.push("DISTINCT ")?;
        }
        Ok(())
    }

    /// An aggregate's `ORDER BY`, the closing parenthesis, then `FILTER`
    /// and `OVER` (`get_agg_expr`, `get_windowfunc_expr`).
    fn call_close(&self, expression: &Expr<'_>, output: &mut Output<'_>) -> Result<(), SqlError> {
        let Expr::Call {
            order_by,
            filter,
            over,
            ..
        } = expression
        else {
            return output.unsupported();
        };
        if !order_by.is_empty() {
            output.push(" ORDER BY ")?;
            for (index, order) in order_by.iter().enumerate() {
                if index != 0 {
                    output.push(", ")?;
                }
                let natural = self.type_of(order.expression)?;
                self.write(order.expression, natural, output)?;
                if order.descending {
                    output.push(" DESC")?;
                    if !order.nulls_first {
                        output.push(" NULLS LAST")?;
                    }
                } else if order.nulls_first {
                    output.push(" NULLS FIRST")?;
                }
            }
        }
        output.push(")")?;
        self.call_suffix(*filter, *over, output)
    }

    fn call_suffix(
        &self,
        filter: Option<&Expr<'_>>,
        over: Option<&crate::sql::ast::WindowSpec<'_>>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        if let Some(filter) = filter {
            output.push(" FILTER (WHERE ")?;
            self.write(filter, Some(oid::BOOL), output)?;
            output.push(")")?;
        }
        if let Some(window) = over {
            output.push(" OVER ")?;
            match window.name {
                Some(name) => push_identifier(output, name)?,
                None => query::window_spec(self, window, output)?,
            }
        }
        Ok(())
    }

    /// PostgreSQL's grammar flattens a left-nested `AND`/`OR` chain into one
    /// list; a right operand of the same connective stays a nested list.
    fn connective(
        &self,
        expression: &Expr<'_>,
        operator: BinaryOp,
        first: bool,
        descend: bool,
        output: &mut Output<'_>,
    ) -> Result<bool, SqlError> {
        if let Expr::Binary {
            operator: found,
            left,
            right,
            ..
        } = expression
            && *found == operator
            && descend
        {
            // A `BETWEEN`'s connective is its own list.
            let nested = !left.source_position().is_expanded();
            let first = self.connective(left, operator, first, nested, output)?;
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
        let (left_type, right_type) = self.binary_operands(BinaryOp::Like, left, right)?;
        self.open(output)?;
        let parent = Parent::Operator(symbol);
        self.wrapped(left, Some(left_type), parent, true, output)?;
        output.push(" ")?;
        output.push(symbol)?;
        output.push(" ")?;
        self.wrapped(right, Some(right_type), parent, false, output)?;
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
        Parent::Not => false,
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
        | Expr::BitLit(_)
        | Expr::Subscript { .. }
        | Expr::Slice { .. }
        | Expr::Array(_)
        | Expr::SchemaColumn { .. }
        | Expr::Case {
            origin: crate::sql::ast::CaseOrigin::Written,
            ..
        }
        | Expr::Cast { .. } => true,
        Expr::Call {
            kind: CallKind::SqlSyntax,
            name: crate::sql::parser::SIMILAR_TO,
            ..
        } => operator("~"),
        Expr::Call {
            kind: CallKind::SqlSyntax,
            name: crate::sql::parser::ROWS_NOT_DISTINCT,
            ..
        } => connective(None),
        Expr::Call { .. } => true,
        Expr::Unary {
            operator: UnaryOp::Neg,
            operand: Expr::Int(_) | Expr::NumericLit(_),
            ..
        } => true,
        Expr::Unary {
            operator: UnaryOp::Not,
            operand,
            source_position,
        } if source_position.is_expanded() => match operand {
            Expr::Call {
                args: [left, _], ..
            } if row_items(left).is_some_and(|items| items.len() > 1) => {
                connective(Some(BinaryOp::Or))
            }
            Expr::Call {
                name: crate::sql::parser::SIMILAR_TO,
                ..
            } => operator("!~"),
            _ => matches!(parent, Parent::Connective(_) | Parent::Not),
        },
        Expr::Unary {
            operator: UnaryOp::Not,
            ..
        } => connective(None),
        Expr::Unary { .. } => operator("prefix"),
        Expr::Binary {
            operator: kind @ (BinaryOp::And | BinaryOp::Or),
            ..
        } => connective(Some(*kind)),
        Expr::Binary {
            operator: kind,
            left,
            ..
        } if row_items(left).is_some() => match kind {
            BinaryOp::Eq if row_items(left).is_some_and(|items| items.len() > 1) => {
                connective(Some(BinaryOp::And))
            }
            BinaryOp::NotEq if row_items(left).is_some_and(|items| items.len() > 1) => {
                connective(Some(BinaryOp::Or))
            }
            BinaryOp::Eq | BinaryOp::NotEq => operator("="),
            _ => false,
        },
        Expr::Binary { operator: kind, .. } => kind.operator_name().is_some_and(operator),
        Expr::InList {
            operand,
            list,
            negated,
        } => {
            let (array, count) = operators::in_list_shape(list);
            if row_items(operand).is_some() {
                if list.len() > 1 {
                    connective(Some(if *negated {
                        BinaryOp::And
                    } else {
                        BinaryOp::Or
                    }))
                } else {
                    false
                }
            } else if count > 1 {
                connective(Some(if *negated {
                    BinaryOp::And
                } else {
                    BinaryOp::Or
                }))
            } else if array {
                false
            } else {
                operator("=")
            }
        }
        Expr::Like { .. } | Expr::Match { .. } => operator("~~"),
        Expr::IsNull { .. }
        | Expr::Exists(_)
        | Expr::Subquery(_)
        | Expr::ArraySubquery(_)
        | Expr::InSubquery { .. }
        | Expr::QuantifiedSubquery { .. }
        | Expr::Case {
            origin:
                crate::sql::ast::CaseOrigin::BooleanTest { .. }
                | crate::sql::ast::CaseOrigin::UnknownTest { .. }
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

/// The fields of a row constructor, `ROW(...)` or `(a, b)`.
fn row_items<'e>(expression: &'e Expr<'e>) -> Option<&'e [&'e Expr<'e>]> {
    match expression {
        Expr::Call {
            kind: CallKind::SqlSyntax,
            name,
            args,
            ..
        } if name.eq_ignore_ascii_case("row") => Some(args),
        _ => None,
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

/// `pg_get_viewdef(view)`: PostgreSQL's indented layout of the view's query
/// with analyzed expressions, or `None` for a shape outside this module's
/// coverage. A stored view's relations, routines and types are bound to the
/// identities they captured at CREATE and printed under their current names.
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
    let writer = query::Writer {
        storage,
        txid,
        pretty: layout.pretty,
        dependencies,
        qualify: origin == crate::storage::ViewQueryOrigin::ReturnRule,
    };
    let mut output = Output::new(arena);
    output.wrap = layout.wrap_column;
    output.layout = true;
    writer.query(
        &[],
        select,
        query::Naming {
            names: Some(column_names),
            visible: true,
            unknown: false,
        },
        false,
        arena,
        &mut output,
    )?;
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
    rule_table: Columns<'_>,
    action: &'a str,
    pretty: bool,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    use crate::sql::ast::Stmt;
    let Ok(statement) = crate::sql::parser::parse_stored_statement(action, arena) else {
        return Ok(None);
    };
    let mut output = Output::new(arena);
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
    let (target, from, returning) = match statement {
        Stmt::Insert(insert) => (
            Some((insert.table.schema, insert.table.name, insert.alias)),
            None,
            insert.returning,
        ),
        Stmt::Update(update) => (
            Some((update.table.schema, update.table.name, update.alias)),
            update.from,
            update.returning,
        ),
        Stmt::Delete(delete) => (
            Some((delete.table.schema, delete.table.name, delete.alias)),
            delete.using,
            delete.returning,
        ),
        Stmt::Select(_) => (None, None, crate::sql::ast::Returning::NONE),
        _ => return Ok(None),
    };
    let read = match target {
        Some((schema, table, _)) => {
            let Some(crate::storage::ResolvedRelation::Table(slot)) =
                storage.resolve_relation(schema, table, txid)
            else {
                return Ok(None);
            };
            Some(storage.table_def(slot, txid))
        }
        None => None,
    };
    let target_table = read.as_deref();
    // OLD and NEW, then the target and, for ON CONFLICT DO UPDATE, EXCLUDED.
    let mut rows = [Joined::rule_columns("old", rule_table); 4];
    rows[1] = Joined::rule_columns("new", rule_table);
    let mut count = 2;
    if let (Some(table), Some((_, _, alias))) = (target_table, target) {
        rows[2] = Joined::new(alias.unwrap_or(table.name.as_str()), table);
        count = 3;
        if matches!(statement, Stmt::Insert(insert) if insert.on_conflict.is_some()) {
            rows[3] = Joined::rule_row("excluded", table);
            count = 4;
        }
    }
    let writer = query::Writer {
        storage,
        txid,
        pretty,
        dependencies: None,
        qualify: true,
    };
    output.layout = true;
    // UPDATE's FROM and DELETE's USING items join the target's level.
    let joined = match from {
        Some(from) => match writer.dml_scope(&rows[..count], from, arena)? {
            Some(scope) => scope,
            None => return Ok(None),
        },
        None => &rows[..count],
    };
    let context = Context {
        storage,
        txid,
        arena,
        dependencies: None,
        table: target_table,
        parameters: &[],
        joined,
        pretty,
        qualify: true,
        domain_value: None,
        bare: None,
    };
    // The target's relation, then its alias after `separator`.
    let relation = |separator: &str| {
        let table = target_table.expect("statement has a target");
        let exposed = target.and_then(|(_, _, alias)| alias);
        let mut relation = crate::util::StackStr::<400>::new();
        if !storage.schema_is_on_path(table.schema, txid) {
            write_identifier(&mut relation, table.schema.as_str());
            let _ = relation.write_char('.');
        }
        write_identifier(&mut relation, table.name.as_str());
        if let Some(alias) = exposed.filter(|alias| *alias != table.name.as_str()) {
            let _ = relation.write_str(separator);
            write_identifier(&mut relation, alias);
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
            output.keyword(" WHERE ", -8, 8, 1)?;
            context.write(condition, Some(oid::BOOL), output)?;
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
    // `get_insert_query_def` and its siblings indent their clauses.
    output.indent = if target.is_some() { 8 } else { 0 };
    match statement {
        Stmt::Insert(insert) => {
            let target = target_table.expect("INSERT has a target");
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
            output.push(relation(" AS ").as_str())?;
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
                    output.keyword("VALUES (", -8, 8, 2)?;
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
                    // rule's rows, not the target.
                    // The INSERT level's target keeps its name reserved.
                    let mut around = [rows[0], rows[1], rows[2]];
                    around[2].unqualified = false;
                    around[2].addressable = false;
                    around[2].level = 1;
                    writer.query(
                        &around,
                        select,
                        query::Naming {
                            names: None,
                            visible: false,
                            unknown: true,
                        },
                        true,
                        arena,
                        &mut output,
                    )?;
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
            let target = target_table.expect("UPDATE has a target");
            output.push(" UPDATE ")?;
            output.push(relation(" ").as_str())?;
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
            if let Some(from) = update.from {
                writer.dml_from(&context, joined, from, " FROM ", arena, &mut output)?;
            }
            condition(&mut output, update.where_clause)?;
        }
        Stmt::Delete(delete) => {
            output.push(" DELETE FROM ")?;
            output.push(relation(" ").as_str())?;
            if let Some(from) = delete.using {
                writer.dml_from(&context, joined, from, " USING ", arena, &mut output)?;
            }
            condition(&mut output, delete.where_clause)?;
        }
        Stmt::Select(select) => {
            writer.query(
                &rows[..2],
                select,
                query::Naming {
                    names: None,
                    visible: true,
                    unknown: false,
                },
                false,
                arena,
                &mut output,
            )?;
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

/// A string constant of type `unknown`: quoted, without a type label.
fn quote_literal(output: &mut Output<'_>, text: &str) -> Result<(), SqlError> {
    output.push("'")?;
    for (index, part) in text.split('\'').enumerate() {
        if index != 0 {
            output.push("''")?;
        }
        output.push(part)?;
    }
    output.push("'")
}

/// A function name without the `pg_catalog.` the parser may attach.
fn unqualified(name: &str) -> &str {
    name.strip_prefix("pg_catalog.").unwrap_or(name)
}

fn push_identifier(output: &mut Output<'_>, name: &str) -> Result<(), SqlError> {
    let mut identifier = crate::util::StackStr::<130>::new();
    write_identifier(&mut identifier, name);
    output.push(identifier.as_str())
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
