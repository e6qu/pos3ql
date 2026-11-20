//! The keys of a grouping of one table's rows as the planner model numbers
//! them and as `estimate_num_groups` reads them: a column, an expression an
//! index or statistics object holds (`examine_variable` finds its
//! statistics), or a free expression of the table's columns.
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/selfuncs.c

use crate::mem::arena::Arena;
use crate::sql::ast::Expr;
use crate::sql::eval::SqlError;
use crate::sql::types::ColType;
use crate::storage::{ColumnSet, Storage};

use super::super::scope::{QueryScope, ResolvedColumn};
use super::expressions::ExpressionVariables;

/// Free grouping keys are numbered from here, past every column and
/// expression variable, so no index, condition or statistics matches them.
pub(crate) const FREE_GROUP_KEY: usize = 1 << 20;

/// The most distinct free expressions one grouping reads, its `ORDER BY`
/// included.
const MAX_FREE_KEYS: usize = 16;

/// One grouping key as `estimate_num_groups` reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GroupVariable {
    /// A free expression of type `boolean` reading `columns`: two groups,
    /// whatever it reads.
    Boolean(ColumnSet),
    /// A column, or an expression an index or statistics object holds: one
    /// variable while it has statistics (or a unique index), else the
    /// `columns` its expression reads.
    Variable { variable: usize, columns: ColumnSet },
    /// A free expression: the columns it reads.
    Columns(ColumnSet),
}

/// A grouping key resolved by [`GroupKeys::resolve`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupKey {
    /// Its number: a column, an expression variable, or past
    /// [`FREE_GROUP_KEY`].
    pub(crate) number: usize,
    pub(crate) variable: GroupVariable,
    pub(crate) ctype: ColType,
    /// It is an expression rather than a column.
    pub(crate) expression: bool,
}

/// Resolves the grouping and ordering expressions of a query of the one
/// plain table of `scope`, numbering equal free expressions alike.
pub(crate) struct GroupKeys<'s, 'a> {
    storage: &'s Storage,
    txid: u32,
    scope: &'s QueryScope<'a>,
    columns: &'s dyn crate::sql::exec::ColTypeResolver,
    variables: Option<ExpressionVariables<'a>>,
    free: [Option<&'a Expr<'a>>; MAX_FREE_KEYS],
    free_count: usize,
    arena: &'a Arena,
}

impl<'s, 'a> GroupKeys<'s, 'a> {
    /// `None` when `scope` is no query of one plain table.
    pub(crate) fn new(
        storage: &'s Storage,
        txid: u32,
        scope: &'s QueryScope<'a>,
        columns: &'s dyn crate::sql::exec::ColTypeResolver,
        arena: &'a Arena,
    ) -> Result<Option<Self>, SqlError> {
        if scope.n != 1
            || scope.slots[0] == usize::MAX
            || scope.defs.stored(0).is_none()
            || u16::try_from(scope.slots[0])
                .map_or(true, |slot| storage.foreign_table(slot, txid).is_some())
        {
            return Ok(None);
        }
        Ok(Some(Self {
            storage,
            txid,
            scope,
            columns,
            variables: super::query_variables(storage, txid, scope, arena)?,
            free: [None; MAX_FREE_KEYS],
            free_count: 0,
            arena,
        }))
    }

    /// The key `expression` is, or `None` when it is outside the model: no
    /// column of the table, a constant, or an expression calling an
    /// aggregate, a window or set-returning function or a volatile routine,
    /// reading a subquery, or of a type without a column layout.
    pub(crate) fn resolve(
        &mut self,
        expression: &'a Expr<'a>,
    ) -> Result<Option<GroupKey>, SqlError> {
        let definition = self.scope.defs.stored(0).expect("a plain table");
        let expression = super::without_own_type_cast(expression, self.scope);
        if let Expr::Column { qualifier, name } = expression {
            return Ok(match self.scope.find_column(*qualifier, name) {
                Ok(ResolvedColumn::Table(0, column)) => Some(GroupKey {
                    number: column,
                    variable: GroupVariable::Variable {
                        variable: column,
                        columns: ColumnSet::from_column(column),
                    },
                    ctype: definition.columns()[column].ctype,
                    expression: false,
                }),
                _ => None,
            });
        }
        if expression.contains_subquery()
            || super::super::expr_has_window(expression)
            || super::super::expr_has_aggregate(expression, self.storage, self.txid, self.columns)?
            || super::super::srf::expression_has_project_set(expression, self.storage, self.txid)
            || super::super::predicate_safety::expression_properties(
                expression,
                self.columns,
                self.storage,
                self.txid,
                super::super::predicate_safety::AnalysisStage::RuntimeProjection,
                self.arena,
            )?
            .volatile
        {
            return Ok(None);
        }
        // The columns it reads (`pull_var_clause`), every one the table's.
        let mut read = ColumnSet::EMPTY;
        let mut foreign = false;
        expression.for_each_column_reference(&mut |qualifier, name| match self
            .scope
            .find_column(qualifier, name)
        {
            Ok(ResolvedColumn::Table(0, column)) => read.insert(column),
            _ => foreign = true,
        });
        if foreign || read.is_empty() {
            return Ok(None);
        }
        let Ok((type_oid, _)) = crate::sql::exec::infer_type_catalog(
            expression,
            Some(definition),
            self.storage,
            self.txid,
            self.arena,
        ) else {
            return Ok(None);
        };
        let Some((ctype, None)) =
            crate::sql::exec::catalog_column_type(self.storage, self.txid, type_oid)
        else {
            return Ok(None);
        };
        if let Some(variables) = self.variables
            && let Some(variable) = variables.find(self.scope, expression, self.arena)?
        {
            return Ok(Some(GroupKey {
                number: variable,
                variable: GroupVariable::Variable {
                    variable,
                    columns: read,
                },
                ctype,
                expression: true,
            }));
        }
        let resolve =
            |qualifier: Option<&str>, name: &str| match self.scope.find_column(qualifier, name) {
                Ok(ResolvedColumn::Table(0, column)) => Some(column),
                _ => None,
            };
        let Some(canonical) =
            super::expressions::canonical(expression, &resolve, definition, self.arena)?
        else {
            return Ok(None);
        };
        let at = match self.free[..self.free_count]
            .iter()
            .position(|held| held.is_some_and(|held| *held == *canonical))
        {
            Some(at) => at,
            None if self.free_count == MAX_FREE_KEYS => return Ok(None),
            None => {
                self.free[self.free_count] = Some(canonical);
                self.free_count += 1;
                self.free_count - 1
            }
        };
        Ok(Some(GroupKey {
            number: FREE_GROUP_KEY + at,
            variable: if ctype == ColType::Bool {
                GroupVariable::Boolean(read)
            } else {
                GroupVariable::Columns(read)
            },
            ctype,
            expression: true,
        }))
    }
}
