//! Stored type fields share one walk for renames and journal publication.

use super::{
    CastDef, ColType, OperatorClassDefinition, OperatorDefinition, OperatorFamilyDefinition,
    OperatorFamilyFunction, OperatorFamilyOperator, OperatorImplementation, RoutineArgumentDef,
    RoutineDef, RoutineKind, RoutineParameterDef, RoutineResult, Storage, UserTypeName,
};
use crate::sql::eval::SqlError;
use core::convert::Infallible;

pub(crate) enum CatalogTypes<'a> {
    Routine(&'a mut RoutineDef),
    Cast(&'a mut CastDef),
    Operator(&'a mut OperatorDefinition),
    Family(&'a mut OperatorFamilyDefinition),
    Class(&'a mut OperatorClassDefinition),
}

pub(crate) trait CatalogTypeDefinition {
    fn catalog_types(&mut self) -> CatalogTypes<'_>;
}

macro_rules! catalog_type_definition {
    ($definition:ty, $variant:ident) => {
        impl CatalogTypeDefinition for $definition {
            fn catalog_types(&mut self) -> CatalogTypes<'_> {
                CatalogTypes::$variant(self)
            }
        }
    };
}
catalog_type_definition!(CastDef, Cast);
catalog_type_definition!(RoutineDef, Routine);
catalog_type_definition!(OperatorDefinition, Operator);
catalog_type_definition!(OperatorFamilyDefinition, Family);
catalog_type_definition!(OperatorClassDefinition, Class);

fn member_types<E>(
    operators: &mut [OperatorFamilyOperator],
    functions: &mut [OperatorFamilyFunction],
    visit: &mut impl FnMut(&mut RoutineResult) -> Result<(), E>,
) -> Result<(), E> {
    for member in operators.iter_mut().filter(|member| member.used) {
        visit(&mut member.left)?;
        visit(&mut member.right)?;
    }
    for member in functions.iter_mut().filter(|member| member.used) {
        visit(&mut member.left)?;
        visit(&mut member.right)?;
    }
    Ok(())
}

impl CatalogTypes<'_> {
    pub(super) fn try_for_each<E>(
        self,
        mut visit: impl FnMut(&mut RoutineResult) -> Result<(), E>,
    ) -> Result<(), E> {
        match self {
            Self::Routine(definition) => {
                routine_type_fields(
                    &mut definition.arguments[..definition.argument_count],
                    &mut definition.parameters[..definition.parameter_count],
                    &mut definition.result_columns[..definition.result_column_count],
                    &mut definition.kind,
                    &mut visit,
                )?;
                if let Some(pending) = &mut definition.pending_definition {
                    routine_type_fields(
                        &mut pending.arguments[..pending.argument_count],
                        &mut pending.parameters[..pending.parameter_count],
                        &mut pending.result_columns[..pending.result_column_count],
                        &mut pending.kind,
                        &mut visit,
                    )?;
                }
            }
            Self::Cast(definition) => {
                visit(&mut definition.source)?;
                visit(&mut definition.target)?;
            }
            Self::Operator(definition) => {
                for argument in [
                    &mut definition.signature.left,
                    &mut definition.signature.right,
                ]
                .into_iter()
                .flatten()
                {
                    visit(argument)?;
                }
                if let OperatorImplementation::Function { result, .. } =
                    &mut definition.implementation
                {
                    visit(result)?;
                }
            }
            Self::Family(definition) => member_types(
                &mut definition.operators,
                &mut definition.functions,
                &mut visit,
            )?,
            Self::Class(definition) => {
                visit(&mut definition.input)?;
                visit(&mut definition.storage)?;
                member_types(
                    &mut definition.operators,
                    &mut definition.functions,
                    &mut visit,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn for_each(self, mut visit: impl FnMut(&mut RoutineResult)) {
        let result: Result<(), Infallible> = self.try_for_each(|value| {
            visit(value);
            Ok(())
        });
        match result {
            Ok(()) => {}
            Err(never) => match never {},
        }
    }
}

fn type_pair<E>(
    ctype: &mut ColType,
    name: &mut Option<UserTypeName>,
    visit: &mut impl FnMut(&mut RoutineResult) -> Result<(), E>,
) -> Result<(), E> {
    let mut result = RoutineResult {
        ctype: *ctype,
        user_type: *name,
    };
    visit(&mut result)?;
    *ctype = result.ctype;
    *name = result.user_type;
    Ok(())
}

pub(super) fn routine_type_fields<E>(
    arguments: &mut [RoutineArgumentDef],
    parameters: &mut [RoutineParameterDef],
    columns: &mut [RoutineArgumentDef],
    kind: &mut RoutineKind,
    visit: &mut impl FnMut(&mut RoutineResult) -> Result<(), E>,
) -> Result<(), E> {
    for argument in arguments.iter_mut().chain(columns.iter_mut()) {
        type_pair(&mut argument.ctype, &mut argument.user_type, visit)?;
    }
    for parameter in parameters {
        type_pair(&mut parameter.ctype, &mut parameter.user_type, visit)?;
    }
    match kind {
        RoutineKind::Function { result } | RoutineKind::SetFunction { result } => visit(result)?,
        RoutineKind::Aggregate(aggregate) => {
            visit(&mut aggregate.state_type)?;
            visit(&mut aggregate.result_type)?;
            if let Some(moving) = &mut aggregate.moving {
                visit(&mut moving.state_type)?;
            }
        }
        RoutineKind::RecordFunction { .. }
        | RoutineKind::TableFunction
        | RoutineKind::Trigger
        | RoutineKind::EventTrigger
        | RoutineKind::Procedure => {}
    }
    Ok(())
}

impl Storage {
    /// Validate every type field and refresh its name from the captured source.
    /// Journal images also use this walk before discarding process-local keys.
    pub(crate) fn bind_catalog_definition<T: CatalogTypeDefinition>(
        &self,
        mut definition: T,
        txid: u32,
    ) -> Result<T, SqlError> {
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        definition.catalog_types().try_for_each(|result| {
            *result = catalog.bind_type_reference(*result, super::current_database(), txid)?;
            Ok::<(), SqlError>(())
        })?;
        Ok(definition)
    }
}
