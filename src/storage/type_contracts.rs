//! Stored type fields share one walk for renames and journal publication.

use super::{
    CastDef, OperatorClassDefinition, OperatorDefinition, OperatorFamilyDefinition,
    OperatorFamilyFunction, OperatorFamilyOperator, OperatorImplementation, RoutineResult, Storage,
    UserTypeName,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use core::convert::Infallible;

pub(crate) enum CatalogTypes<'a> {
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

impl Storage {
    /// Freeze current names before a full catalog image reaches the journal.
    /// Existing catalog images can still hold the committed spelling while the
    /// same transaction has a pending type rename.
    pub(crate) fn journal_catalog_definition<T: CatalogTypeDefinition>(
        &self,
        mut definition: T,
        txid: u32,
    ) -> Result<T, SqlError> {
        definition
            .catalog_types()
            .try_for_each(|result| -> Result<(), SqlError> {
                let Some(identity) = result.user_type else {
                    return Ok(());
                };
                if result.polymorphic_type().is_some() {
                    return Ok(());
                }
                if let Some(slot) = self.domain_identity_slot(
                    identity.schema.as_str(),
                    identity.name.as_str(),
                    txid,
                ) {
                    let domain = self.domain_for(slot, txid);
                    result.user_type = Some(UserTypeName {
                        schema: domain.schema,
                        name: domain.name,
                    });
                    return Ok(());
                }
                let canonical = self
                    .routine_type_oid(result.ctype, result.user_type, txid)
                    .and_then(|oid| self.routine_result_for_oid(oid, txid))
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "stored type \"{}.{}\" does not exist",
                            identity.schema.as_str(),
                            identity.name.as_str()
                        )
                    })?;
                result.user_type = canonical.user_type;
                Ok(())
            })?;
        Ok(definition)
    }
}
