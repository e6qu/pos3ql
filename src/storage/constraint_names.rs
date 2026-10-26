//! Generated constraint names become durable before a definition is installed.

use super::{DomainSpec, SqlName, Storage, TableDef};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;
use core::fmt::Write;

fn generated(left: &str, right: Option<&str>, label: &str) -> Result<SqlName, SqlError> {
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

impl TableDef {
    pub(crate) fn set_not_null_constraint_name(&mut self, column: usize, name: SqlName) {
        self.not_null_names[column] = Some(name);
        self.not_null_name_edits.insert(column);
    }
}

pub(crate) fn table(definition: &mut TableDef) -> Result<(), SqlError> {
    for position in 0..definition.n_columns {
        let column = definition.columns[position];
        if !column.not_null.is_required() {
            definition.not_null_names[position] = None;
        } else if definition.not_null_names[position].is_none() {
            let name = choose(
                definition.name.as_str(),
                Some(column.name.as_str()),
                |name| {
                    definition.not_null_names[..definition.n_columns].contains(&Some(name))
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
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "constraint \"{}\" already exists",
                name.as_str()
            ));
        }
    }
    definition.not_null_name_edits = super::ColumnSet::EMPTY;
    Ok(())
}

pub(crate) fn remap_table(
    previous: &TableDef,
    definition: &mut TableDef,
    mapping: &[Option<SqlName>; super::MAX_COLUMNS],
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
            names[target] = previous.not_null_names[column];
        }
    }
    for (column, name) in names[..definition.n_columns].iter_mut().enumerate() {
        if definition.not_null_name_edits.contains(column) {
            *name = definition.not_null_names[column];
        }
    }
    definition.not_null_names = names;
    table(definition)
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
    if spec.not_null_name.is_some_and(|name| {
        spec.checks[..spec.n_checks]
            .iter()
            .any(|constraint| constraint.name == name)
    }) {
        return Err(sql_err!(
            sqlstate::DUPLICATE_OBJECT,
            "domain constraint name already exists"
        ));
    }
    Ok(())
}

impl Storage {
    pub(crate) fn validate_not_null_names(definition: &TableDef) -> Result<(), SqlError> {
        for (position, column) in definition.columns().iter().enumerate() {
            if column.not_null.is_required() != definition.not_null_names[position].is_some()
                || definition.not_null_names[position].is_some_and(|name| name.as_str().is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ColumnMeta, MAX_COLUMNS, NotNullOrigin};

    #[test]
    fn catalog_snapshot_not_null_names_follow_columns_through_compaction() {
        let mut original = TableDef::empty();
        original.name = SqlName::parse("original").unwrap();
        original.n_columns = 2;
        original.columns[0].name = SqlName::parse("removed").unwrap();
        original.columns[1].name = SqlName::parse("retained").unwrap();
        original.columns[1].not_null = NotNullOrigin::Local;
        table(&mut original).unwrap();
        let retained_name = original.not_null_names[1];
        let mut replacement = original;
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
            remap_table(&original, &mut replacement, &mapping).unwrap();
            assert_eq!(replacement.not_null_names[0], retained_name);
            assert_eq!(
                replacement.not_null_names[1].unwrap().as_str(),
                "renamed_removed_not_null"
            );
            Storage::validate_not_null_names(&replacement).unwrap();
            assert_eq!(original.not_null_names[0], None);
            assert_eq!(original.not_null_names[1], retained_name);
        });
    }
}
