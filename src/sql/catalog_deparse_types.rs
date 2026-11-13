//! The engine's expression typing (`infer_type_res`) over a deparse scope,
//! for the expression shapes the deparser's own typing does not model.

use super::{Columns, Context};
use crate::sql::SqlError;
use crate::sql::ast::Expr;
use crate::sql::exec::{CatalogCols, ColTypeResolver, StaticTypeMeta};
use crate::sql::types::ColType;

/// A deparse scope's columns, with catalog answers from [`CatalogCols`].
pub(super) struct ScopeTypes<'c, 'a> {
    pub context: &'c Context<'c>,
    pub catalog: CatalogCols<'a>,
}

impl ScopeTypes<'_, '_> {
    fn meta(&self, qualifier: Option<&str>, name: &str) -> Option<StaticTypeMeta> {
        if let Some((source, column)) = self.context.joined_source(qualifier, name) {
            return match source.columns {
                Columns::Table(table) => crate::sql::exec::static_meta_for_column(
                    self.context.storage,
                    &table.columns()[column],
                    self.context.txid,
                ),
                Columns::Derived(columns) => {
                    let type_oid = columns[column].type_oid?;
                    let ctype = ColType::from_oid(type_oid).or_else(|| {
                        self.context
                            .storage
                            .domain_base_type_for_oid(type_oid, self.context.txid)
                    })?;
                    Some(StaticTypeMeta {
                        ctype,
                        type_oid,
                        type_mod: -1,
                        collation: crate::sql::ast::Collation::None,
                    })
                }
            };
        }
        let table = self.context.table?;
        if qualifier.is_some_and(|qualifier| qualifier != table.name.as_str()) {
            return None;
        }
        let column = table.columns().get(table.column_index(name)?)?;
        crate::sql::exec::static_meta_for_column(self.context.storage, column, self.context.txid)
    }
}

impl ColTypeResolver for ScopeTypes<'_, '_> {
    fn validate_enum_input_literal(&self, text: &str, target_oid: i32) -> Result<(), SqlError> {
        self.catalog.validate_enum_input_literal(text, target_oid)
    }

    fn type_name_fixed(
        &self,
        type_oid: i32,
    ) -> Result<Option<crate::util::StackStr<260>>, SqlError> {
        self.catalog.type_name_fixed(type_oid)
    }

    fn operator_first_argument_type(
        &self,
        name: &str,
        left: i32,
        right: i32,
    ) -> Result<Option<StaticTypeMeta>, SqlError> {
        self.catalog.operator_first_argument_type(name, left, right)
    }

    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        self.meta(qualifier, name)
            .map(|meta| meta.ctype)
            .ok_or_else(|| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                )
            })
    }

    fn column_meta(&self, qualifier: Option<&str>, name: &str) -> Option<StaticTypeMeta> {
        self.meta(qualifier, name)
    }

    fn validate_explicit_cast(
        &self,
        source_oid: i32,
        target_oid: i32,
        target: ColType,
    ) -> Result<(), SqlError> {
        self.catalog
            .validate_explicit_cast(source_oid, target_oid, target)
    }

    fn domain_base_type(&self, type_oid: i32) -> Option<ColType> {
        self.catalog.domain_base_type(type_oid)
    }

    fn subquery_type(&self, expression: &Expr<'_>) -> Result<Option<(i32, i16)>, SqlError> {
        let select = match expression {
            Expr::Subquery(select) | Expr::ArraySubquery(select) => *select,
            _ => return Ok(None),
        };
        let Some(type_oid) = super::query::sublink_type(self.context, select)? else {
            return Ok(None);
        };
        let type_oid = match expression {
            Expr::ArraySubquery(_) => match super::array_of(type_oid) {
                Some(array) => array,
                None => return Ok(None),
            },
            _ => type_oid,
        };
        Ok(Some((
            type_oid,
            ColType::from_oid(type_oid).map_or(-1, ColType::typlen),
        )))
    }

    fn named_type_oid(&self, type_name: &str) -> Option<i32> {
        self.catalog.named_type_oid(type_name)
    }

    fn routine_call_selection(
        &self,
        name: &str,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
    ) -> Option<crate::storage::RoutineCallSelection> {
        self.catalog.routine_call_selection(
            name,
            kind,
            bound_oid,
            argument_names,
            variadic,
            arguments,
        )
    }

    fn routine_result(
        &self,
        name: &str,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
    ) -> Option<StaticTypeMeta> {
        self.catalog
            .routine_result(name, kind, bound_oid, argument_names, variadic, arguments)
    }

    fn operator_result(&self, name: &str, left_oid: i32, right_oid: i32) -> Option<StaticTypeMeta> {
        self.catalog.operator_result(name, left_oid, right_oid)
    }

    fn routine_record_field(
        &self,
        name: &str,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        self.catalog.routine_record_field(
            name,
            kind,
            bound_oid,
            argument_names,
            variadic,
            arguments,
            index,
        )
    }

    fn composite_field(
        &self,
        slot: Option<crate::storage::TypeCreation>,
        field: &str,
    ) -> Option<StaticTypeMeta> {
        self.catalog.composite_field(slot, field)
    }

    fn visit_composite_fields(
        &self,
        slot: Option<crate::storage::TypeCreation>,
        visit: &mut dyn FnMut(&str, StaticTypeMeta),
    ) -> Option<usize> {
        self.catalog.visit_composite_fields(slot, visit)
    }

    fn visit_named_composite_fields(
        &self,
        type_name: &str,
        visit: &mut dyn FnMut(&str, StaticTypeMeta),
    ) -> Result<Option<usize>, SqlError> {
        self.catalog.visit_named_composite_fields(type_name, visit)
    }
}

impl Context<'_> {
    /// The engine's type for `expression` under this scope, when it has one.
    pub(super) fn inferred_type(&self, expression: &Expr<'_>) -> Option<i32> {
        let types = ScopeTypes {
            context: self,
            catalog: CatalogCols {
                definition: None,
                alias: None,
                output_aliases: &[],
                storage: self.storage,
                txid: self.txid,
                arena: self.arena,
            },
        };
        crate::sql::exec::infer_type_res(expression, &types)
            .ok()
            .map(|(type_oid, _)| type_oid)
            .filter(|type_oid| *type_oid != crate::sql::types::oid::UNKNOWN)
    }
}

impl Context<'_> {
    /// The output columns of a FROM function whose result is a composite
    /// or record type: its `OUT` parameters, or its row type's fields.
    /// `None` when they cannot be determined.
    pub(super) fn record_fields(
        &self,
        call: &Expr<'_>,
        result: i32,
        visit: &mut dyn FnMut(&str, i32) -> Result<(), SqlError>,
    ) -> Result<Option<usize>, SqlError> {
        let Expr::Call {
            kind,
            name,
            args,
            argument_names,
            variadic,
            bound_oid,
            ..
        } = call
        else {
            return Ok(None);
        };
        if args.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
            return Ok(None);
        }
        let mut arguments =
            [crate::sql::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (target, argument) in arguments.iter_mut().zip(*args) {
            *target = self
                .type_of(argument)?
                .unwrap_or(crate::sql::types::oid::UNKNOWN);
        }
        let types = ScopeTypes {
            context: self,
            catalog: CatalogCols {
                definition: None,
                alias: None,
                output_aliases: &[],
                storage: self.storage,
                txid: self.txid,
                arena: self.arena,
            },
        };
        let mut count = 0;
        while let Some((field, meta)) = types.routine_record_field(
            name,
            *kind,
            *bound_oid,
            argument_names,
            *variadic,
            &arguments[..args.len()],
            count,
        ) {
            visit(field.as_str(), meta.type_oid)?;
            count += 1;
        }
        if count > 0 {
            return Ok(Some(count));
        }
        let Some(type_name) =
            crate::sql::catalog::user_type_name_fixed(self.storage, self.txid, result)?
        else {
            return Ok(None);
        };
        let mut failure = Ok(());
        let found = crate::sql::exec::visit_catalog_rowtype_fields(
            self.storage,
            self.txid,
            type_name.as_str(),
            self.arena,
            &mut |field, meta| {
                if failure.is_ok() {
                    failure = visit(field, meta.type_oid);
                }
            },
        )?;
        failure?;
        Ok(found)
    }
}
