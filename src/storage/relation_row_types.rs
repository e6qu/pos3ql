//! Every table, view and materialized view owns a composite row type.
//!
//! PostgreSQL stores the row type as a `pg_type` row whose `typrelid` names the
//! relation; its attributes are the relation's own `pg_attribute` rows. pos3ql
//! keeps that single source of truth: a row type is identified by its owning
//! relation's durable creation stamp, which shares the named-type identifier
//! space, and its fields are read from the relation definition on demand.

use super::type_definitions::{TypeDefinitionRead, TypeDefinitionTarget};
use super::type_identities::TypeCreation;
use super::{
    ArrElem, ColType, CompositeDef, CompositeFieldDef, NamedTypeKind, RoutineResult, SqlName,
    Storage, TableDefinitionRead, UserTypeName, column_attributes::AttributeSlot,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// The relation whose row type a composite creation stamp identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowTypeRelation {
    /// Tables, partitioned tables, foreign tables and materialized views.
    Table(usize),
    View(usize),
}

/// One composite type's physical field layout, whether it was declared with
/// `CREATE TYPE ... AS` or belongs to a relation.
pub(crate) enum RowTypeDefinition<'s> {
    Named(TypeDefinitionRead<CompositeDef>),
    Table(TableDefinitionRead<'s>),
}

impl RowTypeDefinition<'_> {
    pub(crate) fn visible_to(&self, txid: u32) -> bool {
        match self {
            Self::Named(definition) => definition.visible_to(txid),
            Self::Table(_) => true,
        }
    }

    /// The type's committed name, for diagnostics.
    pub(crate) fn name(&self) -> SqlName {
        match self {
            Self::Named(definition) => definition.name,
            Self::Table(definition) => definition.name,
        }
    }

    /// Physical attribute count, including dropped attributes.
    pub(crate) fn physical_field_count(&self) -> usize {
        match self {
            Self::Named(definition) => definition.n_fields,
            Self::Table(definition) => usize::from(definition.attributes.count),
        }
    }

    /// The field at a zero-based physical position.
    pub(crate) fn field(&self, position: usize) -> CompositeFieldDef {
        match self {
            Self::Named(definition) => definition.fields()[position],
            Self::Table(definition) => {
                let number = u16::try_from(position + 1).expect("relation attribute fits");
                match definition.attributes.slots.as_ref()[position] {
                    AttributeSlot::Live(column) => {
                        let column = definition.columns()[usize::from(column)];
                        CompositeFieldDef {
                            attribute_number: number,
                            name: column.name,
                            ctype: column.ctype,
                            type_mod: column.type_mod,
                            collation: column.collation,
                            user_type: column.user_type,
                            dropped: false,
                            not_null: column.not_null.is_required(),
                        }
                    }
                    AttributeSlot::Empty | AttributeSlot::Dropped { .. } => CompositeFieldDef {
                        attribute_number: number,
                        ..CompositeFieldDef::EMPTY
                    },
                }
            }
        }
    }

    /// The physical fields as one transaction sees them, including its own
    /// uncommitted attribute changes.
    pub(crate) fn fields_for(&self, txid: u32) -> impl Iterator<Item = CompositeFieldDef> + '_ {
        let named = match self {
            Self::Named(definition) => Some(definition.fields_for(txid)),
            Self::Table(_) => None,
        };
        let count = named.map_or(self.physical_field_count(), <[_]>::len);
        (0..count).map(move |position| {
            named.map_or_else(|| self.field(position), |fields| fields[position])
        })
    }

    pub(crate) fn active_fields_for(
        &self,
        txid: u32,
    ) -> impl Iterator<Item = CompositeFieldDef> + '_ {
        self.fields_for(txid).filter(|field| !field.dropped)
    }

    pub(crate) fn fields(&self) -> impl Iterator<Item = CompositeFieldDef> + '_ {
        (0..self.physical_field_count()).map(|position| self.field(position))
    }

    pub(crate) fn active_fields(&self) -> impl Iterator<Item = CompositeFieldDef> + '_ {
        self.fields().filter(|field| !field.dropped)
    }

    pub(crate) fn active_field_count(&self) -> usize {
        self.active_fields().count()
    }
}

impl Storage {
    /// The visible relation owning the row type created at `creation`.
    pub(crate) fn row_type_relation(
        &self,
        creation: TypeCreation,
        txid: u32,
    ) -> Option<RowTypeRelation> {
        let created_at = creation.created_at();
        if let Some(slot) = (0..self.table_count()).find(|slot| {
            self.table_slot_visible_to(*slot, txid) && self.table(*slot).created_at() == created_at
        }) {
            return Some(RowTypeRelation::Table(slot));
        }
        (0..self.view_count())
            .find(|slot| {
                self.view_slot_visible_to(*slot, txid)
                    && self.view_catalog_identity(*slot).created_at() == created_at
            })
            .map(RowTypeRelation::View)
    }

    /// The relation's current schema and name, which are also its row type's.
    pub(crate) fn row_type_relation_name(
        &self,
        relation: RowTypeRelation,
        txid: u32,
    ) -> (SqlName, SqlName) {
        match relation {
            RowTypeRelation::Table(slot) => {
                let definition = self.table_def(slot, txid);
                (definition.schema, definition.name)
            }
            RowTypeRelation::View(slot) => {
                let view = self.view(slot);
                (view.schema_for(txid), view.name_for(txid))
            }
        }
    }

    /// The row type identity of a relation slot.
    pub(crate) fn relation_row_type_creation(&self, relation: RowTypeRelation) -> TypeCreation {
        let created_at = match relation {
            RowTypeRelation::Table(slot) => self.table(slot).created_at(),
            RowTypeRelation::View(slot) => self.view_catalog_identity(slot).created_at(),
        };
        TypeCreation::parse(created_at).expect("admitted relation stamp is a row type identity")
    }

    /// The field layout of any composite type. A view's columns come from its
    /// stored query, so only callers that can describe a query accept them;
    /// every other caller receives an explicit `0A000`.
    pub(crate) fn row_type_definition(
        &self,
        creation: TypeCreation,
        txid: u32,
    ) -> Result<Option<RowTypeDefinition<'_>>, SqlError> {
        if let Ok(target) =
            TypeDefinitionTarget::by_creation(self, NamedTypeKind::Composite, creation)
        {
            return Ok(self
                .read_composite_definition(target, txid)?
                .map(RowTypeDefinition::Named));
        }
        match self.row_type_relation(creation, txid) {
            Some(RowTypeRelation::Table(slot)) => {
                Ok(Some(RowTypeDefinition::Table(self.table_def(slot, txid))))
            }
            Some(RowTypeRelation::View(slot)) => Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "values of the row type of view \"{}\" are not supported",
                self.view(slot).name_for(txid).as_str()
            )),
            None => Ok(None),
        }
    }

    /// Like [`Self::row_type_definition`], but a missing type is an error.
    pub(crate) fn require_row_type_definition(
        &self,
        creation: TypeCreation,
        txid: u32,
    ) -> Result<RowTypeDefinition<'_>, SqlError> {
        self.row_type_definition(creation, txid)?
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "composite type does not exist"))
    }

    /// The OID of a visible relation row type or its array type.
    pub(crate) fn relation_row_type_oid(
        &self,
        creation: TypeCreation,
        array: bool,
        txid: u32,
    ) -> Option<i32> {
        use crate::sql::types::oid;
        self.row_type_relation(creation, txid)?;
        Some(if array {
            oid::composite_array_oid(creation)
        } else {
            oid::composite_oid(creation)
        })
    }

    /// The declared type of a relation row type, named as the relation is now.
    pub(crate) fn bind_relation_row_type(
        &self,
        creation: TypeCreation,
        array: bool,
        txid: u32,
    ) -> Result<RoutineResult, SqlError> {
        let relation = self.row_type_relation(creation, txid).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "relation row type no longer exists"
            )
        })?;
        let (schema, name) = self.row_type_relation_name(relation, txid);
        Ok(RoutineResult {
            ctype: if array {
                ColType::Array(ArrElem::Composite(Some(creation)))
            } else {
                ColType::Composite(Some(creation))
            },
            user_type: Some(UserTypeName::relation_row_type(schema, name, creation)),
        })
    }

    /// The declared type of a relation's row type.
    pub(crate) fn relation_row_type_result(
        &self,
        relation: RowTypeRelation,
        array: bool,
        txid: u32,
    ) -> Result<RoutineResult, SqlError> {
        self.bind_relation_row_type(self.relation_row_type_creation(relation), array, txid)
    }

    /// Resolves a possibly qualified name to a visible relation row type.
    pub(crate) fn resolve_relation_row_type(
        &self,
        type_name: &str,
        txid: u32,
    ) -> Option<RowTypeRelation> {
        let (qualifier, name) = type_name
            .split_once('.')
            .map_or((None, type_name), |(schema, name)| (Some(schema), name));
        match self.resolve_relation(qualifier, name, txid)? {
            super::ResolvedRelation::Table(slot) => Some(RowTypeRelation::Table(slot)),
            super::ResolvedRelation::View(slot) => Some(RowTypeRelation::View(slot)),
            super::ResolvedRelation::Catalog => None,
        }
    }
}
