//! Generated constraint names become durable before a definition is installed.

use super::{DomainSpec, SqlName, Storage};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;
use core::fmt::Write;

pub(crate) fn generated(left: &str, right: Option<&str>, label: &str) -> Result<SqlName, SqlError> {
    let right = right.unwrap_or("");
    let overhead = label.len() + 1 + usize::from(!right.is_empty());
    let available = 63usize.checked_sub(overhead).ok_or_else(|| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "constraint name label is too long"
        )
    })?;
    let (mut first, mut second) = (left.len(), right.len());
    while first + second > available {
        if first > second {
            first -= 1;
        } else {
            second -= 1;
        }
    }
    while !left.is_char_boundary(first) {
        first -= 1;
    }
    while !right.is_char_boundary(second) {
        second -= 1;
    }
    let mut name = StackStr::<64>::new();
    write!(name, "{}", &left[..first]).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "constraint name exceeds its reservation"
        )
    })?;
    if second != 0 {
        write!(name, "_{}", &right[..second]).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "constraint name exceeds its reservation"
            )
        })?;
    }
    write!(name, "_{label}").map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "constraint name exceeds its reservation"
        )
    })?;
    SqlName::parse(name.as_str())
}

fn choose(
    left: &str,
    right: Option<&str>,
    occupied: impl Fn(SqlName) -> bool,
) -> Result<SqlName, SqlError> {
    for suffix in 0..=u16::MAX {
        let mut label = StackStr::<32>::from_str("not_null");
        if suffix != 0 {
            write!(label, "{suffix}").expect("bounded constraint suffix fits");
        }
        let name = generated(left, right, label.as_str())?;
        if !occupied(name) {
            return Ok(name);
        }
    }
    Err(sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "NOT NULL constraint names are exhausted"
    ))
}

impl super::TableDefBuilder {
    pub(crate) fn set_not_null_constraint_name(&mut self, column: usize, name: SqlName) {
        self.not_null_names[column] = Some(name);
        self.not_null_name_edits.insert(column);
    }
}

/// Where a NOT NULL constraint name an altered table generates must also be
/// free.
#[derive(Clone, Copy)]
pub(crate) enum GeneratedNames<'n> {
    /// Free in the table and among the schema's other constraints, which
    /// `taken` reports, as PostgreSQL's `ChooseConstraintName` requires.
    InSchema(&'n dyn Fn(SqlName) -> bool),
    /// None: a stored or already admitted definition names every constraint.
    Assigned,
}

/// Names each required column's NOT NULL constraint that has none, as ALTER
/// TABLE does, and rejects a name another constraint of the table holds.
pub(crate) fn table(
    definition: &mut super::TableDefBuilder,
    generated: GeneratedNames<'_>,
) -> Result<(), SqlError> {
    for position in 0..definition.n_columns {
        let column = definition.columns[position];
        if !column.not_null.is_required() {
            definition.not_null_names[position] = None;
        } else if definition.not_null_names[position].is_none() {
            let GeneratedNames::InSchema(schema_taken) = generated else {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "stored NOT NULL constraint name is missing or unexpected"
                ));
            };
            let name = choose(
                definition.name.as_str(),
                Some(column.name.as_str()),
                |name| {
                    schema_taken(name)
                        || definition.not_null_names[..definition.n_columns].contains(&Some(name))
                        || definition
                            .inline_key_names()
                            .iter()
                            .any(|key| key.name == name)
                        || definition
                            .checks()
                            .iter()
                            .any(|constraint| constraint.name == name)
                        || definition
                            .uniques()
                            .iter()
                            .any(|constraint| constraint.name == name)
                        || definition
                            .fkeys()
                            .iter()
                            .any(|constraint| constraint.name == name)
                        || definition
                            .exclusions()
                            .iter()
                            .any(|constraint| constraint.name == name)
                        || definition
                            .partition
                            .detached_bound
                            .is_some_and(|constraint| constraint.name == name)
                },
            )?;
            definition.not_null_names[position] = Some(name);
        } else if let Some(name) = definition.not_null_names[position]
            && (definition
                .inline_key_names()
                .iter()
                .any(|key| key.name == name)
                || definition
                    .checks()
                    .iter()
                    .any(|constraint| constraint.name == name)
                || definition
                    .uniques()
                    .iter()
                    .any(|constraint| constraint.name == name)
                || definition
                    .fkeys()
                    .iter()
                    .any(|constraint| constraint.name == name)
                || definition
                    .exclusions()
                    .iter()
                    .any(|constraint| constraint.name == name)
                || definition.not_null_names[..position].contains(&Some(name)))
        {
            return Err(already_exists(definition, name));
        }
    }
    Ok(())
}

fn already_exists(definition: &super::TableDefBuilder, name: SqlName) -> SqlError {
    sql_err!(
        sqlstate::DUPLICATE_OBJECT,
        "constraint \"{}\" for relation \"{}\" already exists",
        name.as_str(),
        definition.name.as_str()
    )
}

/// A NOT NULL constraint name CREATE TABLE repeats. PostgreSQL stores the new
/// table's CHECK constraints first, then its NOT NULL constraints one at a
/// time, so the repeat fails the catalog's unique index on `pg_constraint`,
/// whose DETAIL names the new table's OID: it is reported once the table has
/// that OID.
#[must_use]
pub(crate) struct CatalogNameClash(SqlName);

impl CatalogNameClash {
    pub(crate) fn error(self, relation_oid: i32) -> SqlError {
        let mut detail = StackStr::<160>::new();
        // A constraint name fits with the fixed wording.
        let _ = write!(
            detail,
            "Key (conrelid, contypid, conname)=({}, 0, {}) already exists.",
            relation_oid as u32,
            self.0.as_str()
        );
        crate::sql::eval::stash_object_diagnostic(
            Some(detail.as_str()),
            crate::sql::eval::ErrorObjects::table_constraint(
                "pg_catalog",
                "pg_constraint",
                "pg_constraint_conrelid_contypid_conname_index",
            ),
        );
        sql_err!(
            sqlstate::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"pg_constraint_conrelid_contypid_conname_index\""
        )
    }
}

/// Names a new table's NOT NULL constraints in PostgreSQL's
/// `AddRelationNotNullConstraints` order: the constraints the statement
/// declares, as `order` lists them and then in column order, then those only
/// inherited. A declared name (in `not_null_name_edits`) repeating an earlier
/// declared one is a 42710 error. An undeclared one is chosen to avoid the
/// table's CHECK constraints, the NOT NULL names chosen before it and
/// `namespace_taken`, the names the schema's other constraints hold. An
/// inherited name is kept unless the statement's own CHECK constraints
/// (`created_check`: those it does not merge into inherited ones) or earlier
/// NOT NULL constraints use it. The first name repeating a constraint stored
/// before it is returned for the caller to report with the table's OID; the
/// remaining columns are still named.
pub(crate) fn created_table(
    definition: &mut super::TableDefBuilder,
    order: &[u16],
    created_check: &dyn Fn(&super::CheckConstraint) -> bool,
    namespace_taken: &dyn Fn(SqlName) -> bool,
) -> Result<Option<CatalogNameClash>, SqlError> {
    let columns = definition.n_columns;
    let declared = definition.not_null_name_edits;
    let mut names = [None; super::MAX_COLUMNS];
    let mut named = super::ColumnSet::EMPTY;
    let mut clash = None;
    let ordered = order
        .iter()
        .map(|&column| (false, usize::from(column)))
        .filter(|&(_, column)| column < columns);
    let local = (0..columns).map(|column| (false, column));
    let inherited = (0..columns).map(|column| (true, column));
    for (inherited_only, position) in ordered.chain(local).chain(inherited) {
        {
            let column = definition.columns[position];
            if named.contains(position)
                || !column.not_null.is_required()
                || column.not_null.is_local() == inherited_only
            {
                continue;
            }
            named.insert(position);
            let stored = |name: SqlName, names: &[Option<SqlName>]| {
                names.contains(&Some(name))
                    || definition
                        .checks()
                        .iter()
                        .any(|constraint| constraint.name == name)
            };
            let proposed = definition.not_null_names[position];
            let name = match proposed {
                Some(name) if !inherited_only && declared.contains(position) => {
                    if clash.is_none()
                        && (0..columns).any(|other| {
                            names[other] == Some(name)
                                && declared.contains(other)
                                && definition.columns[other].not_null.is_local()
                        })
                    {
                        return Err(already_exists(definition, name));
                    }
                    name
                }
                // An inherited name gives way only to the statement's own
                // CHECK constraints and earlier NOT NULL names; repeating an
                // inherited CHECK name fails on the catalog below.
                Some(name)
                    if inherited_only
                        && !names[..columns].contains(&Some(name))
                        && !definition
                            .checks()
                            .iter()
                            .any(|check| check.name == name && created_check(check)) =>
                {
                    name
                }
                _ => choose(
                    definition.name.as_str(),
                    Some(column.name.as_str()),
                    |name| stored(name, &names[..columns]) || namespace_taken(name),
                )?,
            };
            if clash.is_none() && stored(name, &names[..columns]) {
                clash = Some(CatalogNameClash(name));
            }
            names[position] = Some(name);
        }
    }
    definition.not_null_names = names;
    if clash.is_none() {
        // Keys and exclusion constraints are created after the NOT NULL
        // constraints, and fail on a name those already hold.
        for name in names[..columns].iter().flatten() {
            if definition
                .inline_key_names()
                .iter()
                .any(|key| key.name == *name)
                || definition
                    .uniques()
                    .iter()
                    .any(|constraint| constraint.name == *name)
                || definition
                    .fkeys()
                    .iter()
                    .any(|constraint| constraint.name == *name)
                || definition
                    .exclusions()
                    .iter()
                    .any(|constraint| constraint.name == *name)
            {
                return Err(already_exists(definition, *name));
            }
        }
    }
    Ok(clash)
}

pub(crate) fn remap_table<B: super::DefinitionBacking>(
    previous: &super::TableDefinition<B>,
    definition: &mut super::TableDefBuilder,
    mapping: &[Option<SqlName>; super::MAX_COLUMNS],
    generated: GeneratedNames<'_>,
) -> Result<(), SqlError> {
    let mut names = [None; super::MAX_COLUMNS];
    for (column, destination) in mapping[..previous.n_columns].iter().enumerate() {
        if let Some(destination) = destination {
            let target = definition
                .column_index(destination.as_str())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "constraint column mapping has no destination"
                    )
                })?;
            names[target] = previous.not_null_names.as_ref()[column];
        }
    }
    for (column, name) in names[..definition.n_columns].iter_mut().enumerate() {
        if definition.not_null_name_edits.contains(column) {
            *name = definition.not_null_names[column];
        }
    }
    definition.not_null_names = names;
    definition.retain_active_inline_key_names()?;
    table(definition, generated)?;
    // Later admission may remap the same draft again; these names are canonical.
    definition.not_null_name_edits = super::ColumnSet::all(definition.n_columns);
    Ok(())
}

pub(super) fn domain(name: SqlName, spec: &mut DomainSpec) -> Result<(), SqlError> {
    if !spec.not_null {
        spec.not_null_name = None;
    } else if spec.not_null_name.is_none() {
        spec.not_null_name = Some(choose(name.as_str(), None, |name| {
            spec.checks[..spec.n_checks]
                .iter()
                .any(|constraint| constraint.name == name)
        })?);
    }
    if let Some(constraint) = spec.not_null_name
        && spec.checks[..spec.n_checks]
            .iter()
            .any(|check| check.name == constraint)
    {
        return Err(sql_err!(
            sqlstate::DUPLICATE_OBJECT,
            "constraint \"{}\" for domain \"{}\" already exists",
            constraint.as_str(),
            name.as_str()
        ));
    }
    Ok(())
}

impl Storage {
    pub(crate) fn validate_not_null_names<B: super::DefinitionBacking>(
        definition: &super::TableDefinition<B>,
    ) -> Result<(), SqlError> {
        for (position, column) in definition.columns().iter().enumerate() {
            if column.not_null.is_required()
                != definition.not_null_names.as_ref()[position].is_some()
                || definition.not_null_names.as_ref()[position]
                    .is_some_and(|name| name.as_str().is_empty())
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "stored NOT NULL constraint name is missing or unexpected"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;
    use crate::storage::{ColumnMeta, MAX_COLUMNS, NotNullOrigin};

    #[test]
    fn catalog_snapshot_not_null_names_follow_columns_through_compaction() {
        let mut original = super::super::TableDefBuilder::empty();
        original.name = SqlName::parse("original").unwrap();
        original.n_columns = 2;
        original.columns[0].name = SqlName::parse("removed").unwrap();
        original.columns[1].name = SqlName::parse("retained").unwrap();
        original.columns[1].not_null = NotNullOrigin::Local;
        table(&mut original, GeneratedNames::InSchema(&|_| false)).unwrap();
        let retained_name = original.not_null_names[1];
        let mut replacement = original.clone();
        replacement.name = SqlName::parse("renamed").unwrap();
        replacement.columns[0] = original.columns[1];
        replacement.columns[1] = ColumnMeta {
            name: original.columns[0].name,
            not_null: NotNullOrigin::Local,
            ..ColumnMeta::EMPTY
        };
        let mut mapping = [None; MAX_COLUMNS];
        mapping[1] = Some(replacement.columns[0].name);
        crate::mem::guard::forbid_alloc(|| {
            remap_table(
                &original,
                &mut replacement,
                &mapping,
                GeneratedNames::InSchema(&|_| false),
            )
            .unwrap();
            assert_eq!(replacement.not_null_names[0], retained_name);
            assert_eq!(
                replacement.not_null_names[1].unwrap().as_str(),
                "renamed_removed_not_null"
            );
            Storage::validate_not_null_names(&replacement).unwrap();
            assert_eq!(original.not_null_names[0], None);
            assert_eq!(original.not_null_names[1], retained_name);
            let explicit = SqlName::parse("explicit_added_name").unwrap();
            replacement.set_not_null_constraint_name(1, explicit);
            remap_table(
                &original,
                &mut replacement,
                &mapping,
                GeneratedNames::InSchema(&|_| false),
            )
            .unwrap();
            assert_eq!(replacement.not_null_names[0], retained_name);
            assert_eq!(replacement.not_null_names[1], Some(explicit));
            assert_eq!(
                replacement.not_null_name_edits,
                super::super::ColumnSet::all(replacement.n_columns)
            );
        });
    }
}
