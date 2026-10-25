//! Generated constraint names become durable before a definition is installed.

use super::{DomainSpec, SqlName, Storage, TableDef};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;
use core::fmt::Write;

fn generated(left: &str, right: Option<&str>, label: &str) -> Result<SqlName, SqlError> {
    let right = right.unwrap_or("");
    let overhead = label.len() + 1 + usize::from(!right.is_empty());
    let available = 63usize.checked_sub(overhead).ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "constraint name label is too long"))?;
    let (mut first, mut second) = (left.len(), right.len());
    while first + second > available {
        if first > second { first -= 1; } else { second -= 1; }
    }
    while !left.is_char_boundary(first) { first -= 1; }
    while !right.is_char_boundary(second) { second -= 1; }
    let mut name = StackStr::<64>::new();
    write!(name, "{}", &left[..first]).map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "constraint name exceeds its reservation"))?;
    if second != 0 { write!(name, "_{}", &right[..second]).map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "constraint name exceeds its reservation"))?; }
    write!(name, "_{label}").map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "constraint name exceeds its reservation"))?;
    SqlName::parse(name.as_str())
}

fn choose(left: &str, right: Option<&str>, occupied: impl Fn(SqlName) -> bool) -> Result<SqlName, SqlError> {
    for suffix in 0..=u16::MAX {
        let mut label = StackStr::<32>::from_str("not_null");
        if suffix != 0 { write!(label, "{suffix}").expect("bounded constraint suffix fits"); }
        let name = generated(left, right, label.as_str())?;
        if !occupied(name) { return Ok(name); }
    }
    Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "NOT NULL constraint names are exhausted"))
}

pub(crate) fn table(definition: &mut TableDef) -> Result<(), SqlError> {
    for position in 0..definition.n_columns {
        let column = definition.columns[position];
        if !column.not_null.is_required() {
            definition.columns[position].not_null_name = None;
        } else if column.not_null_name.is_none() {
            let name = choose(definition.name.as_str(), Some(column.name.as_str()), |name| {
                definition.columns().iter().any(|column| column.not_null_name == Some(name))
                    || definition.checks().iter().any(|constraint| constraint.name == name)
                    || definition.uniques().iter().any(|constraint| constraint.name == name)
                    || definition.fkeys().iter().any(|constraint| constraint.name == name)
                    || definition.exclusions().iter().any(|constraint| constraint.name == name)
                    || definition.partition.detached_bound.is_some_and(|constraint| constraint.name == name)
            })?;
            definition.columns[position].not_null_name = Some(name);
        } else if let Some(name) = column.not_null_name
            && (definition.checks().iter().any(|constraint| constraint.name == name)
                || definition.uniques().iter().any(|constraint| constraint.name == name)
                || definition.fkeys().iter().any(|constraint| constraint.name == name)
                || definition.exclusions().iter().any(|constraint| constraint.name == name)
                || definition.columns()[..position].iter().any(|column| column.not_null_name == Some(name))) {
            return Err(sql_err!(sqlstate::DUPLICATE_OBJECT, "constraint \"{}\" already exists", name.as_str()));
        }
    }
    Ok(())
}

pub(super) fn domain(name: SqlName, spec: &mut DomainSpec) -> Result<(), SqlError> {
    if !spec.not_null { spec.not_null_name = None; }
    else if spec.not_null_name.is_none() {
        spec.not_null_name = Some(choose(name.as_str(), None,
            |name| spec.checks[..spec.n_checks].iter().any(|constraint| constraint.name == name))?);
    }
    if spec.not_null_name.is_some_and(|name| spec.checks[..spec.n_checks].iter().any(|constraint| constraint.name == name)) {
        return Err(sql_err!(sqlstate::DUPLICATE_OBJECT, "domain constraint name already exists"));
    }
    Ok(())
}

impl Storage {
    pub(crate) fn validate_not_null_names(definition: &TableDef) -> Result<(), SqlError> {
        for column in definition.columns() {
            if column.not_null.is_required() != column.not_null_name.is_some()
                || column.not_null_name.is_some_and(|name| name.as_str().is_empty()) {
                return Err(sql_err!(sqlstate::DATA_EXCEPTION, "stored NOT NULL constraint name is missing or unexpected"));
            }
        }
        Ok(())
    }
}
