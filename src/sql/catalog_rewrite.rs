//! Private retained rule bodies share pg_rewrite's selected tuple and lifetime.
//! They are removed before a catalog table reaches SQL name resolution.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/ruleutils.c

use super::view_rows::{boolean, character, integer, string};
use super::{Arena, Datum, Materialization, SqlError, Storage, SynthTable, arena_full};
use crate::sql::catalog_snapshot::{CatalogScan, selected_image};
use crate::sql::eval::{PrivilegeRelation, sqlstate};
use crate::storage::{CatalogRowImage, RuleDef};
use crate::{sql_err, util::StackStr};
use core::{fmt::Write, num::NonZeroU32};

const PUBLIC_COLUMNS: usize = 7;
const VIEW_DEFINITION: usize = 7;
const RULE_BODY: usize = 8;
pub(super) const PRIVATE_COLUMNS: &[(&str, crate::sql::types::ColType)] = &[
    ("_pos3ql_view_definition", crate::sql::types::ColType::Text),
    (
        "_pos3ql_rule_body",
        crate::sql::types::ColType::Array(crate::sql::types::ArrElem::Text),
    ),
];

fn rule_row<'a>(
    storage: &Storage,
    rule: RuleDef,
    transaction: u32,
    bodies: bool,
    arena: &'a Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    let definition = rule.definition_for(transaction);
    let relation = match definition.target {
        crate::storage::RuleTarget::Table(slot) => super::table_oid(storage, usize::from(slot)),
        crate::storage::RuleTarget::View(slot) => super::view_oid(usize::from(slot)),
    };
    let view = if bodies && definition.name.as_str() == "_RETURN" {
        match definition.target {
            crate::storage::RuleTarget::View(slot) => {
                let query = storage.snapshot_view_query(usize::from(slot), transaction, arena)?;
                arena
                    .alloc_str_display(format_args!("{};", query.sql.as_str()))
                    .map(Datum::Text)
                    .map_err(|_| arena_full())?
            }
            crate::storage::RuleTarget::Table(_) => Datum::Null,
        }
    } else {
        Datum::Null
    };
    let body = if bodies {
        let values = arena
            .alloc_slice_with(usize::from(definition.action_count) + 1, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        values[0] = match definition.condition_sql() {
            Some(condition) => super::text(condition, arena)?,
            None => Datum::Null,
        };
        for (value, action) in values[1..].iter_mut().zip(definition.action_sql()) {
            *value = super::text(action, arena)?;
        }
        Datum::Array {
            element: crate::sql::types::ArrElem::Text,
            raw: crate::sql::array::build(values, arena)?,
        }
    } else {
        Datum::Null
    };
    super::row(
        &[
            Datum::Oid(rule.oid() as u32),
            super::text(definition.name.as_str(), arena)?,
            Datum::Oid(relation as u32),
            Datum::Char(definition.event.catalog_code()),
            Datum::Char(definition.enabled.code()),
            Datum::Bool(matches!(
                definition.mode,
                crate::storage::RewriteMode::Instead
            )),
            Datum::Oid(2618),
            view,
            body,
        ][..if bodies { 9 } else { PUBLIC_COLUMNS }],
        arena,
    )
}

pub(super) fn current<'a>(
    storage: &Storage,
    transaction: u32,
    purpose: Materialization,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let bodies = matches!(purpose, Materialization::History);
    let hidden = if bodies { PRIVATE_COLUMNS } else { &[] };
    let (definition, hidden_columns) =
        super::materialize_with_hidden(super::schema::require("pg_rewrite", false), hidden)?;
    let definition = super::allocate_definition(definition, hidden_columns, arena)?;
    let output = super::catalog_rows(arena, storage.rules_visible_to(transaction).count())?;
    for (target, (_, rule)) in output.iter_mut().zip(storage.rules_visible_to(transaction)) {
        *target = rule_row(storage, rule, transaction, bodies, arena)?;
    }
    Ok(SynthTable {
        def: definition,
        rows: output,
        hidden_columns,
    })
}

pub(super) fn selected<'a>(
    storage: &Storage,
    transaction: u32,
    scan: CatalogScan,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let retained = super::select_catalog_rows(
        storage,
        Some("pg_catalog"),
        "pg_rewrite",
        transaction,
        scan,
        arena,
        |at| current(storage, at, Materialization::History, arena),
    )?;
    let (definition, hidden_columns) =
        super::materialize_with_hidden(super::schema::require("pg_rewrite", false), &[])?;
    let definition = super::allocate_definition(definition, hidden_columns, arena)?;
    let rows = arena
        .alloc_slice_with(retained.rows.len(), |at| {
            &retained.rows[at][..PUBLIC_COLUMNS]
        })
        .map_err(|_| arena_full())?;
    Ok(SynthTable {
        def: definition,
        rows,
        hidden_columns,
    })
}

fn selected_rule<'a>(
    storage: &Storage,
    transaction: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a [Datum<'a>]>, SqlError> {
    let current_row = |at| {
        storage
            .rule_slot_by_oid(oid, at)
            .map(|slot| rule_row(storage, storage.rule(slot), at, true, arena))
            .transpose()
    };
    if storage.commit_snapshot() == u64::MAX {
        return current_row(transaction);
    }
    let relation = super::super::catalog_snapshot_keys::RELATIONS
        .iter()
        .find(|relation| relation.name == "pg_rewrite")
        .expect("rewrite catalog identity");
    let history = storage.catalog_rows_matching(relation.identity(storage.current_database_oid()), storage.commit_snapshot(),
        |key| Ok(matches!(crate::sql::exec::decode_projected_col_record(key, 0, arena)?, Datum::Oid(candidate) if candidate == oid as u32)), arena)?;
    if history.is_empty() {
        return current_row(transaction);
    }
    let key = crate::sql::exec::encode_projected_pub(&[Datum::Oid(oid as u32)], arena)?;
    let image = |at| -> Result<Option<CatalogRowImage<'a>>, SqlError> {
        current_row(at)?
            .map(|values| {
                Ok(CatalogRowImage {
                    bytes: crate::sql::exec::encode_projected_pub(values, arena)?,
                    count: NonZeroU32::new(1).unwrap(),
                    root: super::catalog_row_state(storage, at, "pg_rewrite", key)?.root,
                })
            })
            .transpose()
    };
    let committed = image(0)?;
    let private = if transaction == 0 {
        committed
    } else {
        image(transaction)?
    };
    let chosen = selected_image(
        relation.key,
        key,
        committed,
        private,
        history.first().copied(),
        super::catalog_row_state(storage, transaction, "pg_rewrite", key)?,
        CatalogScan::Index,
    )?;
    let mut rows = chosen.into_iter().flatten();
    let Some(chosen) = rows.next() else {
        return Ok(None);
    };
    // PostgreSQL's SPI lookup returns a definition only for exactly one tuple.
    if rows.next().is_some() {
        return Ok(None);
    }
    if chosen.bytes.get(..2) != Some(9u16.to_le_bytes().as_slice()) {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "retained rewrite body has an invalid shape"
        ));
    }
    let mut failed = None;
    let values = arena
        .alloc_slice_with(9, |at| match crate::sql::exec::decode_projected_col_record(
            chosen.bytes,
            at,
            arena,
        ) {
            Ok(value) => value,
            Err(error) => {
                failed = Some(error);
                Datum::Null
            }
        })
        .map_err(|_| arena_full())?;
    if let Some(error) = failed {
        return Err(error);
    }
    Ok(Some(values))
}

pub(super) fn view_definition<'a>(
    storage: &Storage,
    transaction: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Some(slot) = oid
        .checked_sub(super::view_oid(0))
        .and_then(|slot| usize::try_from(slot).ok())
        .filter(|slot| *slot < storage.view_count())
    else {
        return Ok(None);
    };
    if !storage.view_slot_visible_to(slot, transaction) {
        let retained = super::select_catalog_rows(
            storage,
            Some("pg_catalog"),
            "pg_rewrite",
            transaction,
            CatalogScan::Index,
            arena,
            |at| current(storage, at, Materialization::History, arena),
        )?;
        let mut matching = retained.rows.iter().copied().filter(|values| {
            values[2] == Datum::Oid(oid as u32) && values[1] == Datum::Text("_RETURN")
        });
        let Some(values) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() {
            return Ok(None);
        }
        return match values[VIEW_DEFINITION] {
            Datum::Null => Ok(None),
            _ => Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cache lookup failed for relation {}",
                oid
            )),
        };
    }
    let rule_slot = storage.view_return_rule(slot);
    if rule_slot >= storage.rule_count() {
        return Ok(None);
    }
    let Some(values) = selected_rule(storage, transaction, storage.rule(rule_slot).oid(), arena)?
    else {
        return Ok(None);
    };
    if integer(values[2])? != oid || values[1] != Datum::Text("_RETURN") {
        return Ok(None);
    }
    match values[VIEW_DEFINITION] {
        Datum::Null => Ok(None),
        value => string(value).map(Some),
    }
}

pub(super) fn rule_definition<'a>(
    storage: &Storage,
    transaction: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Some(values) = selected_rule(storage, transaction, oid, arena)? else {
        return Ok(None);
    };
    let relation_oid = integer(values[2])?;
    let (schema, relation) = match crate::sql::query::privilege_relation(
        storage,
        transaction,
        PrivilegeRelation::Oid(relation_oid),
    )? {
        Some(crate::storage::ResolvedRelation::Table(slot)) => {
            let definition = storage.table_def(slot, transaction);
            (definition.schema, definition.name)
        }
        Some(crate::storage::ResolvedRelation::View(slot)) => {
            let view = storage.view(slot);
            (view.schema_for(transaction), view.name_for(transaction))
        }
        _ => {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "could not open relation with OID {}",
                relation_oid
            ));
        }
    };
    let name = crate::sql::eval::quote_ident_str(string(values[1])?, arena)?;
    let schema = crate::sql::eval::quote_ident_str(schema.as_str(), arena)?;
    let relation = crate::sql::eval::quote_ident_str(relation.as_str(), arena)?;
    let event = match character(values[3])? {
        b'1' => "SELECT",
        b'2' => "UPDATE",
        b'3' => "INSERT",
        b'4' => "DELETE",
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid retained rewrite event"
            ));
        }
    };
    let Datum::Array { raw, element } = values[RULE_BODY] else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid retained rewrite body"
        ));
    };
    if crate::sql::array::len(raw) == 0 {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "retained rewrite body has no condition field"
        ));
    }
    let mut body = crate::sql::array::elements(raw, element);
    let mut output = StackStr::<{ crate::storage::RULE_SQL_MAX + 512 }>::new();
    let _ = write!(
        output,
        "CREATE RULE {name} AS ON {event} TO {schema}.{relation}"
    );
    match body.next().expect("body condition was counted") {
        Datum::Null => {}
        value => {
            let _ = write!(output, " WHERE ({})", string(value)?);
        }
    }
    let _ = write!(
        output,
        " DO {}",
        if boolean(values[5])? {
            "INSTEAD "
        } else {
            "ALSO "
        }
    );
    let actions = crate::sql::array::len(raw) - 1;
    if actions == 0 {
        let _ = output.write_str("NOTHING;");
    } else if actions == 1 {
        let _ = write!(output, "{};", string(body.next().expect("counted action"))?);
    } else {
        let _ = output.write_char('(');
        for action in body {
            let _ = write!(output, "{};", string(action)?);
        }
        let _ = output.write_str(");");
    }
    if output.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "formatted rewrite rule exceeds static output capacity"
        ));
    }
    arena
        .alloc_str(output.as_str())
        .map(Some)
        .map_err(|_| arena_full())
}
