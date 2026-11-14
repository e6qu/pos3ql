//! The catalog objects a table's stored expressions (CHECK constraints,
//! column defaults or generation expressions, and exclusion constraint
//! elements and predicates) were bound to when written.
//!
//! PostgreSQL stores these expressions as analyzed trees that name routines,
//! operators and types by OID, so neither a later `search_path` nor a rename
//! changes what they call. pos3ql stores their SQL text and, beside it, the
//! dependency list captured under the creator's path: each entry pairs the
//! name as written with the identity it resolved to. Evaluating or printing an
//! expression binds its text to those identities, under their current names.
//! The lists live in the table definition, which the shared
//! `table_metadata_bytes` pool holds once frozen.

use super::{SqlError, SqlName, StoredQueryDependency, sqlstate};
use crate::sql_err;

/// Dependencies one table's stored expressions may hold in all.
pub(crate) const MAX_TABLE_EXPRESSION_DEPENDENCIES: usize = 64;
/// Dependencies one index's key expressions and predicate may hold in all.
pub(crate) const MAX_INDEX_EXPRESSION_DEPENDENCIES: usize = 16;

/// The stored expression a dependency belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExpressionOwner {
    /// A CHECK constraint, by name (unique among the table's constraints).
    Check(SqlName),
    /// A column's default or generation expression, by column name.
    Column(SqlName),
    /// An index's key expression, by key position.
    IndexKey(u8),
    /// An index's partial-index predicate.
    IndexPredicate,
    /// An exclusion constraint's expression element, by constraint name and
    /// element position.
    ExclusionKey(SqlName, u8),
    /// An exclusion constraint's predicate, by constraint name.
    ExclusionPredicate(SqlName),
}

impl ExpressionOwner {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Check(_) => 1,
            Self::Column(_) => 2,
            Self::IndexKey(_) => 3,
            Self::IndexPredicate => 4,
            Self::ExclusionKey(..) => 5,
            Self::ExclusionPredicate(_) => 6,
        }
    }

    /// Whether a table definition, rather than an index, holds this owner.
    pub(crate) const fn belongs_to_table(self) -> bool {
        matches!(
            self,
            Self::Check(_) | Self::Column(_) | Self::ExclusionKey(..) | Self::ExclusionPredicate(_)
        )
    }
}

/// One dependency of one stored expression: the identity it resolved to and
/// its names, current and as written. The slot is bound again on use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExpressionDependency {
    pub owner: ExpressionOwner,
    pub class: super::DependencyClass,
    pub identity: super::StoredDependencyIdentity,
    pub schema: SqlName,
    pub name: SqlName,
    pub referenced_schema: SqlName,
    pub referenced_name: SqlName,
}

impl ExpressionDependency {
    pub(crate) const EMPTY: Self = Self::new(
        ExpressionOwner::IndexPredicate,
        &StoredQueryDependency::EMPTY,
    );

    pub(crate) const fn new(owner: ExpressionOwner, dependency: &StoredQueryDependency) -> Self {
        Self {
            owner,
            class: dependency.class,
            identity: dependency.identity,
            schema: dependency.schema,
            name: dependency.name,
            referenced_schema: dependency.referenced_schema,
            referenced_name: dependency.referenced_name,
        }
    }

    /// The dependency as a stored query holds it, unbound.
    pub(crate) const fn dependency(&self) -> StoredQueryDependency {
        StoredQueryDependency {
            class: self.class,
            slot: u16::MAX,
            identity: self.identity,
            referenced_columns: super::ColumnSet::EMPTY,
            written_columns: super::ColumnSet::EMPTY,
            schema: self.schema,
            name: self.name,
            referenced_schema: self.referenced_schema,
            referenced_name: self.referenced_name,
        }
    }
}

pub(crate) fn too_many_expression_dependencies() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "a table's stored expressions depend on more than {} catalog objects",
        MAX_TABLE_EXPRESSION_DEPENDENCIES
    )
}

pub(crate) fn too_many_index_expression_dependencies() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "an index's expressions depend on more than {} catalog objects",
        MAX_INDEX_EXPRESSION_DEPENDENCIES
    )
}

impl super::IndexDef {
    pub(crate) fn expression_dependencies(&self) -> &[ExpressionDependency] {
        &self.expression_dependencies[..usize::from(self.n_expression_dependencies)]
    }

    /// The dependencies `owner` was bound to, in capture order.
    pub(crate) fn expression_dependencies_of(
        &self,
        owner: ExpressionOwner,
    ) -> impl Iterator<Item = StoredQueryDependency> + Clone + '_ {
        self.expression_dependencies()
            .iter()
            .filter(move |entry| entry.owner == owner)
            .map(ExpressionDependency::dependency)
    }

    /// Appends `owner`'s dependencies.
    pub(crate) fn push_expression_dependencies(
        &mut self,
        owner: ExpressionOwner,
        dependencies: &[StoredQueryDependency],
    ) -> Result<(), SqlError> {
        let count = usize::from(self.n_expression_dependencies);
        if count + dependencies.len() > MAX_INDEX_EXPRESSION_DEPENDENCIES {
            return Err(too_many_index_expression_dependencies());
        }
        for (slot, dependency) in self.expression_dependencies[count..]
            .iter_mut()
            .zip(dependencies)
        {
            *slot = ExpressionDependency::new(owner, dependency);
        }
        self.n_expression_dependencies = (count + dependencies.len()) as u8;
        Ok(())
    }

    /// Installs a journaled dependency list.
    pub(crate) fn restore_expression_dependencies(
        &mut self,
        entries: impl IntoIterator<Item = ExpressionDependency>,
    ) -> Result<(), SqlError> {
        self.n_expression_dependencies = 0;
        for entry in entries {
            let count = usize::from(self.n_expression_dependencies);
            if entry.owner.belongs_to_table() || count == MAX_INDEX_EXPRESSION_DEPENDENCIES {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "journaled index expression dependencies are malformed"
                ));
            }
            self.expression_dependencies[count] = entry;
            self.n_expression_dependencies += 1;
        }
        Ok(())
    }
}

impl<B: super::DefinitionBacking> super::TableDefinition<B> {
    pub(crate) fn expression_dependencies(&self) -> &[ExpressionDependency] {
        &self.expression_dependencies.as_ref()[..self.n_expression_dependencies]
    }

    /// The dependencies `owner` was bound to, in capture order.
    pub(crate) fn expression_dependencies_of(
        &self,
        owner: ExpressionOwner,
    ) -> impl Iterator<Item = StoredQueryDependency> + Clone + '_ {
        self.expression_dependencies()
            .iter()
            .filter(move |entry| entry.owner == owner)
            .map(ExpressionDependency::dependency)
    }
}

impl ExpressionOwner {
    /// This owner moved to the copy `to` of the constraint it names: an
    /// exclusion constraint's or CHECK constraint's; `None` for any other.
    pub(crate) fn renamed_constraint(self, from: SqlName, to: SqlName) -> Option<Self> {
        match self {
            Self::Check(name) if name == from => Some(Self::Check(to)),
            Self::ExclusionKey(name, position) if name == from => {
                Some(Self::ExclusionKey(to, position))
            }
            Self::ExclusionPredicate(name) if name == from => Some(Self::ExclusionPredicate(to)),
            _ => None,
        }
    }
}

impl super::TableDefBuilder {
    /// Gives the constraint `to` of this definition the dependencies the
    /// constraint `from` of `source` holds, as a copied (inherited, cloned
    /// or `LIKE`-copied) constraint keeps what its expressions were bound to.
    pub(crate) fn copy_constraint_expression_dependencies<B: super::DefinitionBacking>(
        &mut self,
        source: &super::TableDefinition<B>,
        from: SqlName,
        to: SqlName,
    ) -> Result<(), SqlError> {
        self.retain_expression_dependencies(|existing| {
            existing.renamed_constraint(to, to).is_none()
        });
        for entry in source.expression_dependencies() {
            let Some(owner) = entry.owner.renamed_constraint(from, to) else {
                continue;
            };
            if self.n_expression_dependencies == MAX_TABLE_EXPRESSION_DEPENDENCIES {
                return Err(too_many_expression_dependencies());
            }
            self.expression_dependencies[self.n_expression_dependencies] =
                ExpressionDependency { owner, ..*entry };
            self.n_expression_dependencies += 1;
        }
        Ok(())
    }

    /// Replaces `owner`'s dependencies with `dependencies`.
    pub(crate) fn set_expression_dependencies(
        &mut self,
        owner: ExpressionOwner,
        dependencies: &[StoredQueryDependency],
    ) -> Result<(), SqlError> {
        self.retain_expression_dependencies(|existing| existing != owner);
        if self.n_expression_dependencies + dependencies.len() > MAX_TABLE_EXPRESSION_DEPENDENCIES {
            return Err(too_many_expression_dependencies());
        }
        for dependency in dependencies {
            self.expression_dependencies[self.n_expression_dependencies] =
                ExpressionDependency::new(owner, dependency);
            self.n_expression_dependencies += 1;
        }
        Ok(())
    }

    /// Keeps the dependencies whose owner `keep` accepts.
    pub(crate) fn retain_expression_dependencies(
        &mut self,
        mut keep: impl FnMut(ExpressionOwner) -> bool,
    ) {
        let mut kept = 0;
        for index in 0..self.n_expression_dependencies {
            let entry = self.expression_dependencies[index];
            if keep(entry.owner) {
                self.expression_dependencies[kept] = entry;
                kept += 1;
            }
        }
        self.n_expression_dependencies = kept;
    }

    /// Moves an exclusion constraint's element and predicate dependencies to
    /// its new name.
    pub(crate) fn rename_exclusion_expression_owners(&mut self, from: SqlName, to: SqlName) {
        for entry in &mut self.expression_dependencies[..self.n_expression_dependencies] {
            entry.owner = match entry.owner {
                ExpressionOwner::ExclusionKey(name, position) if name == from => {
                    ExpressionOwner::ExclusionKey(to, position)
                }
                ExpressionOwner::ExclusionPredicate(name) if name == from => {
                    ExpressionOwner::ExclusionPredicate(to)
                }
                owner => owner,
            };
        }
    }

    /// Renames the owner of a CHECK constraint's or column's dependencies.
    pub(crate) fn rename_expression_owner(&mut self, from: ExpressionOwner, to: ExpressionOwner) {
        for entry in &mut self.expression_dependencies[..self.n_expression_dependencies] {
            if entry.owner == from {
                entry.owner = to;
            }
        }
    }

    /// Drops the dependencies of expressions the definition no longer has.
    pub(crate) fn prune_expression_dependencies(&mut self) {
        let mut kept = 0;
        for index in 0..self.n_expression_dependencies {
            let entry = self.expression_dependencies[index];
            let live = match entry.owner {
                ExpressionOwner::Check(name) => self.checks[..self.n_checks]
                    .iter()
                    .any(|check| check.name == name),
                ExpressionOwner::Column(name) => self.columns[..self.n_columns]
                    .iter()
                    .any(|column| column.name == name && column.default.expression().is_some()),
                ExpressionOwner::ExclusionKey(name, position) => self.exclusions
                    [..self.n_exclusions]
                    .iter()
                    .any(|exclusion| {
                        exclusion.name == name
                            && matches!(
                                (usize::from(position) < exclusion.len())
                                    .then(|| exclusion.element(usize::from(position))),
                                Some(super::ExclusionElement::Expression(_))
                            )
                    }),
                ExpressionOwner::ExclusionPredicate(name) => self.exclusions[..self.n_exclusions]
                    .iter()
                    .any(|exclusion| exclusion.name == name && exclusion.predicate.is_some()),
                ExpressionOwner::IndexKey(_) | ExpressionOwner::IndexPredicate => false,
            };
            if live {
                self.expression_dependencies[kept] = entry;
                kept += 1;
            }
        }
        self.n_expression_dependencies = kept;
    }
}
