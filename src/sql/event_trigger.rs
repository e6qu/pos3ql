//! Transaction-local object graphs exposed to PostgreSQL event-trigger helpers.

use core::cell::Cell;
use core::fmt::Write as _;

use crate::mem::arena::Arena;
use crate::sql::ast::{DropTable, Stmt};
use crate::sql::catalog;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::txn::DdlUndo;
use crate::sql_err;
use crate::storage::{RoutineKind, Storage};
use crate::util::StackStr;

pub(crate) const MAX_EVENT_OBJECTS: usize = 256;
pub(crate) const MAX_ADDRESS_PARTS: usize = crate::storage::MAX_ROUTINE_ARGUMENTS;
type EventIdentity = StackStr<8192>;
type EventAddressPart = StackStr<512>;

#[derive(Clone, Copy)]
pub(crate) struct BeforeDdl<'a> {
    altered_table: Option<(usize, &'a crate::storage::TableDef)>,
    dependent_drops: [Option<(EventObjectRef, bool)>; MAX_EVENT_OBJECTS],
    dependent_drop_count: usize,
}

impl BeforeDdl<'_> {
    pub(crate) const EMPTY: Self = Self {
        altered_table: None,
        dependent_drops: [None; MAX_EVENT_OBJECTS],
        dependent_drop_count: 0,
    };
}

pub(crate) fn capture_before<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    arena: &'a Arena,
) -> Result<BeforeDdl<'a>, SqlError> {
    let mut before = BeforeDdl::EMPTY;
    let alter_root = match statement {
        Stmt::AlterTable(alter)
            if alter.actions.iter().any(|action| {
                matches!(
                    action,
                    crate::sql::ast::AlterAction::DropColumn { .. }
                        | crate::sql::ast::AlterAction::DropConstraint { .. }
                        | crate::sql::ast::AlterAction::DropNotNull { .. }
                )
            }) =>
        {
            storage
                .resolve_relation(alter.table.schema, alter.table.name, txid)
                .and_then(|relation| match relation {
                    crate::storage::ResolvedRelation::Table(slot) => Some(slot),
                    crate::storage::ResolvedRelation::View(_)
                    | crate::storage::ResolvedRelation::Catalog => None,
                })
        }
        _ => None,
    };
    if let Some(slot) = alter_root {
        let definition = arena
            .alloc(*storage.table_def(slot, txid))
            .map_err(|_| crate::sql::query::arena_full_pub())?;
        before.altered_table = Some((slot, definition));
    }

    let table_is_explicitly_dropped = |slot: usize| {
        let table = storage.table_def(slot, txid);
        match statement {
            Stmt::DropTable(drop) => drop.names.iter().any(|name| {
                name.name == table.name.as_str()
                    && name
                        .schema
                        .is_none_or(|schema| schema == table.schema.as_str())
            }),
            Stmt::DropSchema { names, .. } => {
                names.iter().any(|schema| *schema == table.schema.as_str())
            }
            _ => false,
        }
    };

    for child_slot in 0..storage.table_count() {
        let child = storage.table_def(child_slot, txid);
        if table_is_explicitly_dropped(child_slot) {
            continue;
        }
        for (foreign_key_index, foreign_key) in child.fkeys().iter().enumerate() {
            let Some(parent_slot) = storage.find_visible(
                foreign_key.parent_schema.as_str(),
                foreign_key.parent.as_str(),
                txid,
            ) else {
                continue;
            };
            let affected = if table_is_explicitly_dropped(parent_slot) {
                true
            } else if Some(parent_slot) == alter_root {
                let Stmt::AlterTable(alter) = statement else {
                    unreachable!()
                };
                let parent = storage.table_def(parent_slot, txid);
                alter.actions.iter().any(|action| match action {
                    crate::sql::ast::AlterAction::DropColumn { name, .. } => parent
                        .column_index(name)
                        .is_some_and(|column| foreign_key.parent_cols().contains(&(column as u16))),
                    crate::sql::ast::AlterAction::DropConstraint { name, .. } => {
                        foreign_key_references_named_key(parent, name, foreign_key.parent_cols())
                    }
                    _ => false,
                })
            } else {
                false
            };
            if !affected {
                continue;
            }
            let reference = detached_constraint(
                child,
                catalog::FIRST_FK_OID + child_slot as i32 * 64 + foreign_key_index as i32,
                foreign_key.name.as_str(),
            );
            if before.dependent_drops[..before.dependent_drop_count]
                .iter()
                .flatten()
                .any(|(existing, _)| *existing == reference)
            {
                continue;
            }
            let target = before
                .dependent_drops
                .get_mut(before.dependent_drop_count)
                .ok_or_else(graph_full)?;
            *target = Some((reference, true));
            before.dependent_drop_count += 1;
            for ordinal in 0..4 {
                let reference = foreign_key_trigger_reference(
                    child,
                    child_slot,
                    foreign_key_index,
                    foreign_key,
                    ordinal,
                );
                let target = before
                    .dependent_drops
                    .get_mut(before.dependent_drop_count)
                    .ok_or_else(graph_full)?;
                *target = Some((reference, false));
                before.dependent_drop_count += 1;
            }
        }
    }
    Ok(before)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventObject {
    pub class_id: i32,
    pub object_id: i32,
    pub sub_id: i32,
    pub object_type: StackStr<32>,
    pub schema_name: Option<StackStr<64>>,
    pub object_name: Option<StackStr<64>>,
    pub identity: EventIdentity,
    pub address_names: [EventAddressPart; MAX_ADDRESS_PARTS],
    pub address_name_count: usize,
    pub address_args: [EventAddressPart; MAX_ADDRESS_PARTS],
    pub address_arg_count: usize,
}

impl EventObject {
    pub(crate) const EMPTY: Self = Self {
        class_id: 0,
        object_id: 0,
        sub_id: 0,
        object_type: StackStr::new(),
        schema_name: None,
        object_name: None,
        identity: StackStr::new(),
        address_names: [StackStr::new(); MAX_ADDRESS_PARTS],
        address_name_count: 0,
        address_args: [StackStr::new(); MAX_ADDRESS_PARTS],
        address_arg_count: 0,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DdlCommand {
    reference: EventObjectRef,
    pub command_tag: StackStr<64>,
    pub in_extension: bool,
}

impl DdlCommand {
    pub(crate) const EMPTY: Self = Self {
        reference: EventObjectRef::Empty,
        command_tag: StackStr::new(),
        in_extension: false,
    };

    pub(crate) fn object(self, storage: &Storage, txid: u32) -> Result<EventObject, SqlError> {
        self.reference.materialize(storage, txid)
    }

    pub(crate) const fn addressed(self) -> bool {
        !matches!(self.reference, EventObjectRef::Utility(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DroppedObject {
    reference: EventObjectRef,
    pub original: bool,
    pub normal: bool,
    pub temporary: bool,
}

impl DroppedObject {
    pub(crate) const EMPTY: Self = Self {
        reference: EventObjectRef::Empty,
        original: false,
        normal: false,
        temporary: false,
    };

    pub(crate) fn object(self, storage: &Storage, txid: u32) -> Result<EventObject, SqlError> {
        self.reference.materialize(storage, txid)
    }
}

#[derive(Clone, Copy)]
struct ActiveRows {
    ptr: *const (),
    len: usize,
}

std::thread_local! {
    static DDL_COMMANDS: Cell<Option<ActiveRows>> = const { Cell::new(None) };
    static DROPPED_OBJECTS: Cell<Option<ActiveRows>> = const { Cell::new(None) };
}

pub(crate) struct DdlCommandScope(Option<ActiveRows>);

impl Drop for DdlCommandScope {
    fn drop(&mut self) {
        DDL_COMMANDS.with(|active| active.set(self.0));
    }
}

pub(crate) struct DroppedObjectScope(Option<ActiveRows>);

impl Drop for DroppedObjectScope {
    fn drop(&mut self) {
        DROPPED_OBJECTS.with(|active| active.set(self.0));
    }
}

pub(crate) fn enter_ddl_commands(rows: &[DdlCommand]) -> DdlCommandScope {
    let current = ActiveRows {
        ptr: rows.as_ptr().cast(),
        len: rows.len(),
    };
    DdlCommandScope(DDL_COMMANDS.with(|active| active.replace(Some(current))))
}

pub(crate) fn enter_dropped_objects(rows: &[DroppedObject]) -> DroppedObjectScope {
    let current = ActiveRows {
        ptr: rows.as_ptr().cast(),
        len: rows.len(),
    };
    DroppedObjectScope(DROPPED_OBJECTS.with(|active| active.replace(Some(current))))
}

pub(crate) fn with_ddl_commands<T>(visit: impl FnOnce(&[DdlCommand]) -> T) -> Result<T, SqlError> {
    DDL_COMMANDS.with(|active| {
        let rows = active.get().ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "pg_event_trigger_ddl_commands() can only be called in an event trigger function"
            )
        })?;
        // The matching scope owns the source slice for the whole callback and
        // restores any outer invocation before that slice leaves the stack.
        let rows = unsafe { core::slice::from_raw_parts(rows.ptr.cast::<DdlCommand>(), rows.len) };
        Ok(visit(rows))
    })
}

pub(crate) fn with_dropped_objects<T>(
    visit: impl FnOnce(&[DroppedObject]) -> T,
) -> Result<T, SqlError> {
    DROPPED_OBJECTS.with(|active| {
        let rows = active.get().ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "pg_event_trigger_dropped_objects() can only be called in an event trigger function"
            )
        })?;
        // See `with_ddl_commands`: the pointer never escapes its dynamic scope.
        let rows =
            unsafe { core::slice::from_raw_parts(rows.ptr.cast::<DroppedObject>(), rows.len) };
        Ok(visit(rows))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectRef {
    Table(usize),
    View(usize),
    Routine(usize),
    EventTrigger(usize),
    Trigger(usize),
    Rule(usize),
    MaterializedView(usize),
    Sequence(usize),
    Domain(usize),
    Enum(usize),
    Composite(usize),
    Index(usize),
    Schema(usize),
    Cast(usize),
    Operator(usize),
    OperatorFamily(usize),
    OperatorClass(usize),
    Collation(usize),
    Conversion(usize),
    Policy(usize),
    Statistics(usize),
    Publication(usize),
    Subscription(usize),
    Extension(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventObjectRef {
    Empty,
    Utility(StackStr<32>),
    Primary(ObjectRef),
    TableIndex {
        table: u16,
        position: u16,
        name: StackStr<128>,
    },
    TableRowType(u16),
    TableArrayType(u16),
    ToastRelation(u16),
    ToastIndex(u16),
    TableConstraint {
        table: u16,
        oid: i32,
        name: StackStr<128>,
    },
    ViewRowType(u16),
    ViewArrayType(u16),
    ViewRule(u16),
    DomainArray(u16),
    DomainConstraint {
        domain: u16,
        constraint: u16,
    },
    EnumArray(u16),
    CompositeRelation(u16),
    CompositeArray(u16),
    NamedType {
        oid: i32,
        schema: crate::storage::SqlName,
        name: crate::storage::SqlName,
    },
    RelationColumn {
        relation: ObjectRef,
        attnum: u16,
        name: crate::storage::SqlName,
    },
    DetachedColumn {
        table_oid: i32,
        schema: crate::storage::SqlName,
        table: crate::storage::SqlName,
        column: crate::storage::SqlName,
        attnum: u16,
    },
    DetachedConstraint {
        oid: i32,
        schema: crate::storage::SqlName,
        table: crate::storage::SqlName,
        name: StackStr<128>,
    },
    DetachedIndex {
        oid: i32,
        schema: crate::storage::SqlName,
        name: StackStr<128>,
    },
    DetachedTrigger {
        oid: i32,
        schema: crate::storage::SqlName,
        table: crate::storage::SqlName,
        name: StackStr<128>,
    },
}

impl EventObjectRef {
    fn materialize(self, storage: &Storage, txid: u32) -> Result<EventObject, SqlError> {
        Ok(match self {
            Self::Empty => return Err(graph_full()),
            Self::Utility(object_type) => EventObject {
                object_type,
                ..EventObject::EMPTY
            },
            Self::Primary(reference) => primary_object(storage, txid, reference)?,
            Self::TableIndex {
                table,
                position,
                name,
            } => {
                let table_slot = usize::from(table);
                let table = storage.table_def(table_slot, txid);
                index_object(
                    catalog::index_oid(table_slot, usize::from(position)),
                    name.as_str(),
                    table,
                )?
            }
            Self::TableRowType(slot) => {
                let slot = usize::from(slot);
                let table = storage.table_def(slot, txid);
                base_object(
                    catalog::PG_TYPE_OID,
                    catalog::FIRST_TABLE_COMPOSITE_TYPE_OID + slot as i32,
                    "type",
                    Some(table.schema.as_str()),
                    Some(table.name.as_str()),
                    qualified(table.schema.as_str(), table.name.as_str())?,
                    true,
                )
            }
            Self::TableArrayType(slot) => {
                let slot = usize::from(slot);
                let table = storage.table_def(slot, txid);
                array_type_object(
                    table.schema.as_str(),
                    table.name.as_str(),
                    catalog::FIRST_TABLE_COMPOSITE_ARRAY_TYPE_OID + slot as i32,
                )?
            }
            Self::ToastRelation(slot) | Self::ToastIndex(slot) => {
                let slot = usize::from(slot);
                let table_oid = catalog::user_table_oid(slot);
                let toast_name = crate::stack_format!(64, "pg_toast_{}", table_oid);
                let is_index = matches!(self, Self::ToastIndex(_));
                let name = if is_index {
                    crate::stack_format!(128, "{}_index", toast_name.as_str())
                } else {
                    StackStr::from_str(toast_name.as_str())
                };
                base_object(
                    catalog::PG_CLASS_OID,
                    if is_index {
                        catalog::toast_index_oid(slot)
                    } else {
                        catalog::toast_relation_oid(slot)
                    },
                    if is_index { "index" } else { "toast table" },
                    Some("pg_toast"),
                    Some(name.as_str()),
                    qualified("pg_toast", name.as_str())?,
                    false,
                )
            }
            Self::TableConstraint { table, oid, name } => constraint_object(
                oid,
                name.as_str(),
                storage.table_def(usize::from(table), txid),
            )?,
            Self::ViewRowType(slot) => {
                let slot = usize::from(slot);
                let view = storage.view(slot);
                base_object(
                    catalog::PG_TYPE_OID,
                    catalog::FIRST_VIEW_COMPOSITE_TYPE_OID + slot as i32,
                    "type",
                    Some(view.schema.as_str()),
                    Some(view.name.as_str()),
                    qualified(view.schema.as_str(), view.name.as_str())?,
                    true,
                )
            }
            Self::ViewArrayType(slot) => {
                let slot = usize::from(slot);
                let view = storage.view(slot);
                array_type_object(
                    view.schema.as_str(),
                    view.name.as_str(),
                    catalog::FIRST_VIEW_COMPOSITE_ARRAY_TYPE_OID + slot as i32,
                )?
            }
            Self::ViewRule(slot) => {
                let slot = usize::from(slot);
                let view = storage.view(slot);
                let mut identity = EventIdentity::new();
                write!(
                    identity,
                    "\"_RETURN\" on {}",
                    qualified(view.schema.as_str(), view.name.as_str())?.as_str()
                )
                .map_err(|_| graph_full())?;
                let mut rule = base_object(
                    catalog::PG_REWRITE_OID,
                    storage.rule(storage.view_return_rule(slot)).oid(),
                    "rule",
                    None,
                    None,
                    identity,
                    false,
                );
                rule.address_names[0] = StackStr::from_str(view.schema.as_str());
                rule.address_names[1] = StackStr::from_str(view.name.as_str());
                rule.address_names[2] = StackStr::from_str("_RETURN");
                rule.address_name_count = 3;
                rule
            }
            Self::DomainArray(slot) => {
                let slot = usize::from(slot);
                let domain = storage.domain_for(slot, txid);
                array_type_object(
                    domain.schema.as_str(),
                    domain.name.as_str(),
                    crate::sql::types::oid::domain_array_oid(slot as u16),
                )?
            }
            Self::DomainConstraint { domain, constraint } => {
                let domain_slot = usize::from(domain);
                let constraint_slot = usize::from(constraint);
                let domain = storage.domain_for(domain_slot, txid);
                let check = &domain.checks()[constraint_slot];
                let mut identity = EventIdentity::new();
                write!(
                    identity,
                    "{} on {}",
                    identifier(check.name.as_str()).as_str(),
                    qualified(domain.schema.as_str(), domain.name.as_str())?.as_str()
                )
                .map_err(|_| graph_full())?;
                let mut object = base_object(
                    catalog::PG_CONSTRAINT_OID,
                    catalog::FIRST_DOMAIN_CHECK_OID
                        + domain_slot as i32 * crate::storage::MAX_DOMAIN_CHECKS as i32
                        + constraint_slot as i32,
                    "domain constraint",
                    Some(domain.schema.as_str()),
                    None,
                    identity,
                    false,
                );
                object.address_names[0] = StackStr::from_str(
                    qualified(domain.schema.as_str(), domain.name.as_str())?.as_str(),
                );
                object.address_name_count = 1;
                object.address_args[0] = StackStr::from_str(check.name.as_str());
                object.address_arg_count = 1;
                object
            }
            Self::EnumArray(slot) => {
                let slot = usize::from(slot);
                let enumeration = storage.enum_for(slot, txid);
                array_type_object(
                    enumeration.schema.as_str(),
                    enumeration.name.as_str(),
                    crate::sql::types::oid::enum_array_oid(slot as u16),
                )?
            }
            Self::CompositeRelation(slot) => {
                let slot = usize::from(slot);
                let composite = storage.composite_for(slot, txid);
                base_object(
                    catalog::PG_CLASS_OID,
                    catalog::named_composite_relation_oid(slot),
                    "composite type",
                    Some(composite.schema.as_str()),
                    Some(composite.name.as_str()),
                    qualified(composite.schema.as_str(), composite.name.as_str())?,
                    false,
                )
            }
            Self::CompositeArray(slot) => {
                let slot = usize::from(slot);
                let composite = storage.composite_for(slot, txid);
                array_type_object(
                    composite.schema.as_str(),
                    composite.name.as_str(),
                    crate::sql::types::oid::composite_array_oid(slot as u16),
                )?
            }
            Self::NamedType { oid, schema, name } => {
                let column_type =
                    crate::sql::types::ColType::from_oid(oid).ok_or_else(graph_full)?;
                let identity = match column_type {
                    crate::sql::types::ColType::Array(element) => element.typeof_name(),
                    scalar => scalar.name(),
                };
                base_object(
                    catalog::PG_TYPE_OID,
                    oid,
                    "type",
                    Some(schema.as_str()),
                    Some(name.as_str()),
                    StackStr::from_str(identity),
                    true,
                )
            }
            Self::RelationColumn {
                relation,
                attnum,
                name,
            } => {
                let mut object = primary_object(storage, txid, relation)?;
                let relation_name = object.object_name.ok_or_else(graph_full)?;
                let schema = object.schema_name.ok_or_else(graph_full)?;
                let mut identity = qualified(schema.as_str(), relation_name.as_str())?;
                write!(identity, ".{}", identifier(name.as_str()).as_str())
                    .map_err(|_| graph_full())?;
                object.sub_id = i32::from(attnum);
                object.object_type = StackStr::from_str(match relation {
                    ObjectRef::View(_) => "view column",
                    ObjectRef::MaterializedView(_) => "materialized view column",
                    _ => "table column",
                });
                object.object_name = None;
                object.identity = identity;
                object.address_names = [StackStr::new(); MAX_ADDRESS_PARTS];
                object.address_names[0] = StackStr::from_str(schema.as_str());
                object.address_names[1] = StackStr::from_str(relation_name.as_str());
                object.address_names[2] = StackStr::from_str(name.as_str());
                object.address_name_count = 3;
                object.address_args = [StackStr::new(); MAX_ADDRESS_PARTS];
                object.address_arg_count = 0;
                object
            }
            Self::DetachedColumn {
                table_oid,
                schema,
                table,
                column,
                attnum,
            } => {
                let mut identity = qualified(schema.as_str(), table.as_str())?;
                write!(identity, ".{}", identifier(column.as_str()).as_str())
                    .map_err(|_| graph_full())?;
                let mut object = base_object(
                    catalog::PG_CLASS_OID,
                    table_oid,
                    "table column",
                    Some(schema.as_str()),
                    None,
                    identity,
                    false,
                );
                object.sub_id = i32::from(attnum);
                object.address_names[0] = StackStr::from_str(schema.as_str());
                object.address_names[1] = StackStr::from_str(table.as_str());
                object.address_names[2] = StackStr::from_str(column.as_str());
                object.address_name_count = 3;
                object
            }
            Self::DetachedConstraint {
                oid,
                schema,
                table,
                name,
            } => {
                let detached = crate::storage::TableDef {
                    schema,
                    name: table,
                    ..crate::storage::TableDef::empty()
                };
                constraint_object(oid, name.as_str(), &detached)?
            }
            Self::DetachedIndex { oid, schema, name } => base_object(
                catalog::PG_CLASS_OID,
                oid,
                "index",
                Some(schema.as_str()),
                Some(name.as_str()),
                qualified(schema.as_str(), name.as_str())?,
                false,
            ),
            Self::DetachedTrigger {
                oid,
                schema,
                table,
                name,
            } => {
                let mut identity = EventIdentity::new();
                write!(
                    identity,
                    "{} on {}",
                    identifier(name.as_str()).as_str(),
                    qualified(schema.as_str(), table.as_str())?.as_str()
                )
                .map_err(|_| graph_full())?;
                let mut object = base_object(
                    catalog::PG_TRIGGER_OID,
                    oid,
                    "trigger",
                    Some(schema.as_str()),
                    None,
                    identity,
                    false,
                );
                object.address_names[0] = StackStr::from_str(schema.as_str());
                object.address_names[1] = StackStr::from_str(table.as_str());
                object.address_names[2] = StackStr::from_str(name.as_str());
                object.address_name_count = 3;
                object
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mutation {
    Create,
    Alter,
    Drop,
}

fn mutation(undo: DdlUndo) -> Option<(ObjectRef, Mutation)> {
    use DdlUndo::*;
    Some(match undo {
        Created(slot) => (ObjectRef::Table(slot as usize), Mutation::Create),
        Dropped(slot) => (ObjectRef::Table(slot as usize), Mutation::Drop),
        TableAltered(slot) => (ObjectRef::Table(slot as usize), Mutation::Alter),
        ViewCreated(slot) => (ObjectRef::View(slot as usize), Mutation::Create),
        ViewDropped(slot) => (ObjectRef::View(slot as usize), Mutation::Drop),
        ViewSchemaChanged { slot, .. } => (ObjectRef::View(slot as usize), Mutation::Alter),
        RoutineCreated(slot) => (ObjectRef::Routine(slot as usize), Mutation::Create),
        RoutineDropped(slot) => (ObjectRef::Routine(slot as usize), Mutation::Drop),
        RoutineReplaced { slot, .. } | RoutineIdentityAltered { slot, .. } => {
            (ObjectRef::Routine(slot as usize), Mutation::Alter)
        }
        CastCreated(slot) => (ObjectRef::Cast(slot as usize), Mutation::Create),
        CastDropped(slot) => (ObjectRef::Cast(slot as usize), Mutation::Drop),
        OperatorCreated(slot) => (ObjectRef::Operator(slot as usize), Mutation::Create),
        OperatorAltered { slot, .. } => (ObjectRef::Operator(slot as usize), Mutation::Alter),
        OperatorDropped(slot) => (ObjectRef::Operator(slot as usize), Mutation::Drop),
        OperatorFamilyCreated(slot) => (ObjectRef::OperatorFamily(slot as usize), Mutation::Create),
        OperatorFamilyAltered { slot, .. } => {
            (ObjectRef::OperatorFamily(slot as usize), Mutation::Alter)
        }
        OperatorFamilyDropped(slot) => (ObjectRef::OperatorFamily(slot as usize), Mutation::Drop),
        OperatorClassCreated(slot) => (ObjectRef::OperatorClass(slot as usize), Mutation::Create),
        OperatorClassAltered { slot, .. } => {
            (ObjectRef::OperatorClass(slot as usize), Mutation::Alter)
        }
        OperatorClassDropped(slot) => (ObjectRef::OperatorClass(slot as usize), Mutation::Drop),
        CollationCreated(slot) => (ObjectRef::Collation(slot as usize), Mutation::Create),
        CollationAltered { slot, .. } => (ObjectRef::Collation(slot as usize), Mutation::Alter),
        CollationDropped(slot) => (ObjectRef::Collation(slot as usize), Mutation::Drop),
        ConversionCreated(slot) => (ObjectRef::Conversion(slot as usize), Mutation::Create),
        ConversionAltered { slot, .. } => (ObjectRef::Conversion(slot as usize), Mutation::Alter),
        ConversionDropped(slot) => (ObjectRef::Conversion(slot as usize), Mutation::Drop),
        EventTriggerCreated(slot) => (ObjectRef::EventTrigger(slot as usize), Mutation::Create),
        EventTriggerDropped(slot) => (ObjectRef::EventTrigger(slot as usize), Mutation::Drop),
        EventTriggerAltered { slot, .. } => {
            (ObjectRef::EventTrigger(slot as usize), Mutation::Alter)
        }
        TriggerCreated(slot) => (ObjectRef::Trigger(slot as usize), Mutation::Create),
        TriggerDropped(slot) => (ObjectRef::Trigger(slot as usize), Mutation::Drop),
        TriggerAltered { slot, .. } => (ObjectRef::Trigger(slot as usize), Mutation::Alter),
        RuleCreated { slot, .. } => (ObjectRef::Rule(slot as usize), Mutation::Create),
        RuleDropped(slot) => (ObjectRef::Rule(slot as usize), Mutation::Drop),
        RuleAltered { slot, .. } => (ObjectRef::Rule(slot as usize), Mutation::Alter),
        PolicyCreated(slot) => (ObjectRef::Policy(slot as usize), Mutation::Create),
        PolicyDropped(slot) => (ObjectRef::Policy(slot as usize), Mutation::Drop),
        PolicyAltered { slot, .. } => (ObjectRef::Policy(slot as usize), Mutation::Alter),
        StatisticsCreated(slot) => (ObjectRef::Statistics(slot as usize), Mutation::Create),
        StatisticsDropped(slot) => (ObjectRef::Statistics(slot as usize), Mutation::Drop),
        StatisticsAltered { slot, .. } | StatisticsKeysAltered { slot, .. } => {
            (ObjectRef::Statistics(slot as usize), Mutation::Alter)
        }
        PublicationCreated(slot) => (ObjectRef::Publication(slot as usize), Mutation::Create),
        PublicationDropped(slot) => (ObjectRef::Publication(slot as usize), Mutation::Drop),
        PublicationAltered { slot, .. }
        | PublicationOwnerChanged { slot, .. }
        | PublicationRenamed { slot, .. } => {
            (ObjectRef::Publication(slot as usize), Mutation::Alter)
        }
        SubscriptionCreated(slot) => (ObjectRef::Subscription(slot as usize), Mutation::Create),
        SubscriptionDropped(slot) => (ObjectRef::Subscription(slot as usize), Mutation::Drop),
        SubscriptionEnabled { slot, .. }
        | SubscriptionBootstrapChanged { slot, .. }
        | SubscriptionDefinitionChanged { slot, .. }
        | SubscriptionOwnerChanged { slot, .. }
        | SubscriptionRenamed { slot, .. } => {
            (ObjectRef::Subscription(slot as usize), Mutation::Alter)
        }
        MatviewCreated(slot) => (ObjectRef::MaterializedView(slot as usize), Mutation::Create),
        MatviewDropped(slot) => (ObjectRef::MaterializedView(slot as usize), Mutation::Drop),
        SequenceCreated(slot) => (ObjectRef::Sequence(slot as usize), Mutation::Create),
        SequenceDropped(slot) => (ObjectRef::Sequence(slot as usize), Mutation::Drop),
        SequenceAltered { slot, .. } => (ObjectRef::Sequence(slot as usize), Mutation::Alter),
        DomainCreated(slot) => (ObjectRef::Domain(slot as usize), Mutation::Create),
        DomainDropped(slot) => (ObjectRef::Domain(slot as usize), Mutation::Drop),
        DomainAltered { slot, .. } => (ObjectRef::Domain(slot as usize), Mutation::Alter),
        EnumCreated(slot) => (ObjectRef::Enum(slot as usize), Mutation::Create),
        EnumDropped(slot) => (ObjectRef::Enum(slot as usize), Mutation::Drop),
        EnumAltered { slot, .. } => (ObjectRef::Enum(slot as usize), Mutation::Alter),
        CompositeCreated(slot) => (ObjectRef::Composite(slot as usize), Mutation::Create),
        CompositeDropped(slot) => (ObjectRef::Composite(slot as usize), Mutation::Drop),
        CompositeAltered { slot, .. } => (ObjectRef::Composite(slot as usize), Mutation::Alter),
        IndexCreated(slot) => (ObjectRef::Index(slot as usize), Mutation::Create),
        IndexDropped(slot) => (ObjectRef::Index(slot as usize), Mutation::Drop),
        IndexRenamed { slot, .. } | IndexAltered { slot, .. } => {
            (ObjectRef::Index(slot as usize), Mutation::Alter)
        }
        SchemaCreated(slot) => (ObjectRef::Schema(slot as usize), Mutation::Create),
        SchemaDropped(slot) => (ObjectRef::Schema(slot as usize), Mutation::Drop),
        ExtensionCreated(slot) => (ObjectRef::Extension(slot as usize), Mutation::Create),
        ExtensionDropped(slot) => (ObjectRef::Extension(slot as usize), Mutation::Drop),
        ExtensionAltered { slot, .. } => (ObjectRef::Extension(slot as usize), Mutation::Alter),
        ObjectOwnerChanged { object, .. } => (access_reference(object)?, Mutation::Alter),
        _ => return None,
    })
}

fn identifier(value: &str) -> StackStr<132> {
    crate::sql::types::acl_identifier(value)
}

fn qualified(schema: &str, name: &str) -> Result<EventIdentity, SqlError> {
    let mut value = StackStr::new();
    write!(
        value,
        "{}.{}",
        identifier(schema).as_str(),
        identifier(name).as_str()
    )
    .map_err(|_| graph_full())?;
    (!value.is_truncated())
        .then_some(value)
        .ok_or_else(graph_full)
}

fn graph_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "event-trigger object graph exceeds its startup-sized capacity"
    )
}

fn utility_command_object_type(statement: &Stmt<'_>) -> Option<&'static str> {
    use crate::sql::ast::{DefaultPrivilegeObjectKind, PrivilegeObjectKind, PrivilegeTarget};
    let privilege_type = |target: PrivilegeTarget<'_>| {
        Some(match target {
            PrivilegeTarget::Objects { kind, .. } => match kind {
                PrivilegeObjectKind::Table | PrivilegeObjectKind::AllTablesInSchema => "TABLE",
                PrivilegeObjectKind::Sequence | PrivilegeObjectKind::AllSequencesInSchema => {
                    "SEQUENCE"
                }
                PrivilegeObjectKind::Schema => "SCHEMA",
                PrivilegeObjectKind::Type => "TYPE",
                PrivilegeObjectKind::AllFunctionsInSchema => "FUNCTION",
                PrivilegeObjectKind::Tablespace | PrivilegeObjectKind::Database => return None,
            },
            PrivilegeTarget::Routines { kind, .. } => match kind {
                crate::sql::ast::RoutineTargetKind::Function => "FUNCTION",
                crate::sql::ast::RoutineTargetKind::Procedure => "PROCEDURE",
                crate::sql::ast::RoutineTargetKind::Aggregate => "AGGREGATE",
                crate::sql::ast::RoutineTargetKind::Either => "ROUTINE",
            },
        })
    };
    Some(match statement {
        Stmt::GrantPrivileges { target, .. } | Stmt::RevokePrivileges { target, .. } => {
            privilege_type(*target)?
        }
        Stmt::AlterDefaultPrivileges { action, .. } => match action {
            crate::sql::ast::DefaultPrivilegeAction::Grant { kind, .. }
            | crate::sql::ast::DefaultPrivilegeAction::Revoke { kind, .. } => match kind {
                DefaultPrivilegeObjectKind::Tables => "TABLES",
                DefaultPrivilegeObjectKind::Sequences => "SEQUENCES",
                DefaultPrivilegeObjectKind::Functions => "FUNCTIONS",
                DefaultPrivilegeObjectKind::Types => "TYPES",
                DefaultPrivilegeObjectKind::Schemas => "SCHEMAS",
            },
        },
        _ => return None,
    })
}

fn access_reference(object: crate::storage::AccessObject) -> Option<ObjectRef> {
    use crate::storage::AccessClass;
    let slot = usize::from(object.slot);
    Some(match object.class {
        AccessClass::Table => ObjectRef::Table(slot),
        AccessClass::View => ObjectRef::View(slot),
        AccessClass::MaterializedView => ObjectRef::MaterializedView(slot),
        AccessClass::Sequence => ObjectRef::Sequence(slot),
        AccessClass::Schema => ObjectRef::Schema(slot),
        AccessClass::Domain => ObjectRef::Domain(slot),
        AccessClass::Enum => ObjectRef::Enum(slot),
        AccessClass::Index => ObjectRef::Index(slot),
        AccessClass::Routine => ObjectRef::Routine(slot),
        AccessClass::Composite => ObjectRef::Composite(slot),
        AccessClass::Statistics => ObjectRef::Statistics(slot),
        AccessClass::Extension => ObjectRef::Extension(slot),
        AccessClass::Trigger => ObjectRef::Trigger(slot),
        AccessClass::Tablespace | AccessClass::Database | AccessClass::EventTrigger => return None,
    })
}

fn relation_reference(storage: &Storage, txid: u32, schema: &str, name: &str) -> Option<ObjectRef> {
    if let Some(slot) = storage.matview_slot(schema, name, txid) {
        return Some(ObjectRef::MaterializedView(slot));
    }
    if let Some(slot) = storage.sequence_slot(schema, name, txid) {
        return Some(ObjectRef::Sequence(slot));
    }
    if let Some(slot) = storage.index_slot(schema, name, txid) {
        return Some(ObjectRef::Index(slot));
    }
    match storage.resolve_relation(Some(schema), name, txid)? {
        crate::storage::ResolvedRelation::Table(slot) => Some(ObjectRef::Table(slot)),
        crate::storage::ResolvedRelation::View(slot) => Some(ObjectRef::View(slot)),
        crate::storage::ResolvedRelation::Catalog => None,
    }
}

fn comment_reference(
    storage: &Storage,
    txid: u32,
    slot: usize,
    statement: &Stmt<'_>,
) -> Result<EventObjectRef, SqlError> {
    use crate::storage::CommentClass;
    let (class, schema, name, subid) = storage
        .comment_for_event_trigger(slot, txid)
        .ok_or_else(graph_full)?;
    Ok(match class {
        CommentClass::Relation => {
            let relation = relation_reference(storage, txid, schema.as_str(), name.as_str())
                .ok_or_else(graph_full)?;
            if subid == 0 {
                EventObjectRef::Primary(relation)
            } else {
                let column_name = match relation {
                    ObjectRef::Table(table) => storage
                        .table_def(table, txid)
                        .columns()
                        .get(subid as usize - 1)
                        .map(|column| column.name),
                    ObjectRef::MaterializedView(view_slot) => {
                        let view = storage.matview(view_slot);
                        (0..storage.table_count())
                            .find(|table_slot| {
                                let table = storage.table_def(*table_slot, txid);
                                table.schema == view.schema && table.name == view.name
                            })
                            .and_then(|table_slot| {
                                storage
                                    .table_def(table_slot, txid)
                                    .columns()
                                    .get(subid as usize - 1)
                                    .map(|column| column.name)
                            })
                    }
                    ObjectRef::View(_) => match statement {
                        Stmt::Comment {
                            target: crate::sql::ast::CommentTarget::Column { column, .. },
                            ..
                        } => crate::storage::SqlName::parse(column).ok(),
                        _ => None,
                    },
                    _ => None,
                }
                .ok_or_else(graph_full)?;
                EventObjectRef::RelationColumn {
                    relation,
                    attnum: u16::try_from(subid).map_err(|_| graph_full())?,
                    name: column_name,
                }
            }
        }
        CommentClass::Schema => EventObjectRef::Primary(ObjectRef::Schema(
            storage
                .find_schema_visible(name.as_str(), txid)
                .ok_or_else(graph_full)?,
        )),
        CommentClass::Type => {
            if let Some(slot) = storage.domain_slot(schema.as_str(), name.as_str(), txid) {
                EventObjectRef::Primary(ObjectRef::Domain(slot))
            } else if let Some(slot) = storage.enum_slot(schema.as_str(), name.as_str(), txid) {
                EventObjectRef::Primary(ObjectRef::Enum(slot))
            } else if let Some(slot) = storage.composite_slot(schema.as_str(), name.as_str(), txid)
            {
                EventObjectRef::Primary(ObjectRef::Composite(slot))
            } else {
                if let Some(relation) =
                    relation_reference(storage, txid, schema.as_str(), name.as_str())
                {
                    match relation {
                        ObjectRef::Table(slot) => EventObjectRef::TableRowType(
                            u16::try_from(slot).map_err(|_| graph_full())?,
                        ),
                        ObjectRef::MaterializedView(slot) => {
                            let view = storage.matview(slot);
                            let backing = (0..storage.table_count())
                                .find(|table_slot| {
                                    let table = storage.table_def(*table_slot, txid);
                                    table.schema == view.schema && table.name == view.name
                                })
                                .ok_or_else(graph_full)?;
                            EventObjectRef::TableRowType(
                                u16::try_from(backing).map_err(|_| graph_full())?,
                            )
                        }
                        ObjectRef::View(slot) => EventObjectRef::ViewRowType(
                            u16::try_from(slot).map_err(|_| graph_full())?,
                        ),
                        _ => return Err(graph_full()),
                    }
                } else {
                    let (_, oid) = catalog::builtin_type_identity(name.as_str(), false)
                        .ok_or_else(graph_full)?;
                    EventObjectRef::NamedType { oid, schema, name }
                }
            }
        }
        CommentClass::Extension => EventObjectRef::Primary(ObjectRef::Extension(
            storage
                .extension_slot(name.as_str(), txid)
                .ok_or_else(graph_full)?,
        )),
        CommentClass::Collation => EventObjectRef::Primary(ObjectRef::Collation(
            storage
                .collation_slot(schema.as_str(), name.as_str(), txid)
                .ok_or_else(graph_full)?,
        )),
        CommentClass::Conversion => EventObjectRef::Primary(ObjectRef::Conversion(
            storage
                .conversion_slot(schema.as_str(), name.as_str(), txid)
                .ok_or_else(graph_full)?,
        )),
        CommentClass::Trigger => {
            let target = if subid & (1 << 31) == 0 {
                crate::storage::TriggerTarget::Table(
                    u16::try_from(subid.checked_sub(1).ok_or_else(graph_full)?)
                        .map_err(|_| graph_full())?,
                )
            } else {
                crate::storage::TriggerTarget::View(
                    u16::try_from((subid & !(1 << 31)).checked_sub(1).ok_or_else(graph_full)?)
                        .map_err(|_| graph_full())?,
                )
            };
            EventObjectRef::Primary(ObjectRef::Trigger(
                storage
                    .trigger_slot_on(target, name.as_str(), txid)
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::Rule => {
            let target = if subid & (1 << 31) == 0 {
                crate::storage::RuleTarget::Table(
                    u16::try_from(subid.checked_sub(1).ok_or_else(graph_full)?)
                        .map_err(|_| graph_full())?,
                )
            } else {
                crate::storage::RuleTarget::View(
                    u16::try_from((subid & !(1 << 31)).checked_sub(1).ok_or_else(graph_full)?)
                        .map_err(|_| graph_full())?,
                )
            };
            EventObjectRef::Primary(ObjectRef::Rule(
                storage
                    .rule_slot(target, name.as_str(), txid)
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::Tablespace | CommentClass::Database | CommentClass::EventTrigger => {
            return Err(graph_full());
        }
    })
}

fn names(
    object: &mut EventObject,
    schema: Option<&str>,
    name: Option<&str>,
    identity: EventIdentity,
    address_together: bool,
) {
    object.schema_name = schema.map(StackStr::from_str);
    object.object_name = name.map(StackStr::from_str);
    object.identity = identity;
    match (schema, name, address_together) {
        (Some(_), Some(_), true) => {
            object.address_names[0] = StackStr::from_str(identity.as_str());
            object.address_name_count = 1;
        }
        (Some(schema), Some(name), false) => {
            object.address_names[0] = StackStr::from_str(schema);
            object.address_names[1] = StackStr::from_str(name);
            object.address_name_count = 2;
        }
        (None, Some(name), _) => {
            object.address_names[0] = StackStr::from_str(name);
            object.address_name_count = 1;
        }
        _ => {}
    }
}

fn base_object(
    class_id: i32,
    object_id: i32,
    object_type: &str,
    schema: Option<&str>,
    name: Option<&str>,
    identity: EventIdentity,
    address_together: bool,
) -> EventObject {
    let mut object = EventObject {
        class_id,
        object_id,
        object_type: StackStr::from_str(object_type),
        ..EventObject::EMPTY
    };
    names(&mut object, schema, name, identity, address_together);
    object
}

fn array_type_object(schema: &str, name: &str, oid: i32) -> Result<EventObject, SqlError> {
    let mut identity = qualified(schema, name)?;
    identity.write_str("[]").map_err(|_| graph_full())?;
    let array_name = crate::stack_format!(64, "_{}", name);
    Ok(base_object(
        catalog::PG_TYPE_OID,
        oid,
        "type",
        Some(schema),
        Some(array_name.as_str()),
        identity,
        true,
    ))
}

fn constraint_object(
    oid: i32,
    name: &str,
    table: &crate::storage::TableDef,
) -> Result<EventObject, SqlError> {
    let mut identity = EventIdentity::new();
    write!(
        identity,
        "{} on {}",
        identifier(name).as_str(),
        qualified(table.schema.as_str(), table.name.as_str())?.as_str()
    )
    .map_err(|_| graph_full())?;
    let mut object = base_object(
        catalog::PG_CONSTRAINT_OID,
        oid,
        "table constraint",
        Some(table.schema.as_str()),
        None,
        identity,
        false,
    );
    object.address_names[0] = StackStr::from_str(table.schema.as_str());
    object.address_names[1] = StackStr::from_str(table.name.as_str());
    object.address_names[2] = StackStr::from_str(name);
    object.address_name_count = 3;
    Ok(object)
}

fn index_object(
    oid: i32,
    name: &str,
    table: &crate::storage::TableDef,
) -> Result<EventObject, SqlError> {
    Ok(base_object(
        catalog::PG_CLASS_OID,
        oid,
        "index",
        Some(table.schema.as_str()),
        Some(name),
        qualified(table.schema.as_str(), name)?,
        false,
    ))
}

fn inline_primary_name(table: &crate::storage::TableDef) -> StackStr<128> {
    crate::stack_format!(128, "{}_pkey", table.name.as_str())
}

fn inline_unique_name(
    table: &crate::storage::TableDef,
    column: &crate::storage::ColumnMeta,
) -> StackStr<128> {
    crate::stack_format!(128, "{}_{}_key", table.name.as_str(), column.name.as_str())
}

fn foreign_key_references_named_key(
    table: &crate::storage::TableDef,
    name: &str,
    parent_columns: &[u16],
) -> bool {
    if let Some(key) = table.uniques().iter().find(|key| key.name.as_str() == name) {
        return key.columns() == parent_columns;
    }
    for (index, column) in table.columns().iter().enumerate() {
        let generated = if column.primary {
            Some(inline_primary_name(table))
        } else if column.unique {
            Some(inline_unique_name(table, column))
        } else {
            None
        };
        if generated
            .as_ref()
            .is_some_and(|generated| generated.as_str() == name)
        {
            return parent_columns == [index as u16];
        }
    }
    false
}

fn not_null_name(
    table: &crate::storage::TableDef,
    column: &crate::storage::ColumnMeta,
) -> StackStr<128> {
    crate::stack_format!(
        128,
        "{}_{}_not_null",
        table.name.as_str(),
        column.name.as_str()
    )
}

fn routine_object(storage: &Storage, txid: u32, slot: usize) -> Result<EventObject, SqlError> {
    let routine = storage.routine_for(slot, txid);
    let schema = routine.schema_for(txid);
    let name = routine.name_for(txid);
    let mut identity = qualified(schema.as_str(), name.as_str())?;
    identity.write_char('(').map_err(|_| graph_full())?;
    let mut object = base_object(
        catalog::PG_PROC_OID,
        crate::storage::routine_oid(&routine),
        match routine.kind {
            RoutineKind::Procedure => "procedure",
            RoutineKind::Aggregate(_) => "aggregate",
            _ => "function",
        },
        Some(schema.as_str()),
        None,
        StackStr::new(),
        false,
    );
    object.object_name = None;
    object.address_names[0] = StackStr::from_str(schema.as_str());
    object.address_names[1] = StackStr::from_str(name.as_str());
    object.address_name_count = 2;
    for (index, argument) in routine.arguments().iter().enumerate() {
        if index != 0 {
            identity.write_char(',').map_err(|_| graph_full())?;
        }
        let argument_name = match argument.user_type {
            Some(user) => user_type_name(
                user,
                matches!(argument.ctype, crate::sql::types::ColType::Array(_)),
            )?,
            None => builtin_routine_type_name(argument.ctype)?,
        };
        identity
            .write_str(argument_name.as_str())
            .map_err(|_| graph_full())?;
        object.address_args[index] = argument_name;
        object.address_arg_count += 1;
    }
    identity.write_char(')').map_err(|_| graph_full())?;
    if identity.is_truncated() {
        return Err(graph_full());
    }
    object.identity = identity;
    Ok(object)
}

fn user_type_name(
    user: crate::storage::UserTypeName,
    array: bool,
) -> Result<EventAddressPart, SqlError> {
    let mut written = EventAddressPart::new();
    write!(
        written,
        "{}.{}",
        identifier(user.schema.as_str()).as_str(),
        identifier(user.name.as_str()).as_str()
    )
    .map_err(|_| graph_full())?;
    if array {
        written.write_str("[]").map_err(|_| graph_full())?;
    }
    (!written.is_truncated())
        .then_some(written)
        .ok_or_else(graph_full)
}

fn builtin_routine_type_name(
    column_type: crate::sql::types::ColType,
) -> Result<EventAddressPart, SqlError> {
    let (display, catalog_name) = match column_type {
        crate::sql::types::ColType::Array(element) => {
            (element.typeof_name(), element.to_coltype().catalog_name())
        }
        scalar => (scalar.name(), scalar.catalog_name()),
    };
    let name = if display.trim_end_matches("[]") == catalog_name {
        crate::stack_format!(512, "pg_catalog.{display}")
    } else {
        StackStr::from_str(display)
    };
    (!name.is_truncated())
        .then_some(name)
        .ok_or_else(graph_full)
}

fn routine_result_name(
    result: crate::storage::RoutineResult,
) -> Result<EventAddressPart, SqlError> {
    match result.user_type {
        Some(user) => user_type_name(
            user,
            matches!(result.ctype, crate::sql::types::ColType::Array(_)),
        ),
        None => builtin_routine_type_name(result.ctype),
    }
}

fn operator_object(storage: &Storage, txid: u32, slot: usize) -> Result<EventObject, SqlError> {
    let operator = storage.operator(slot);
    let definition = operator.definition_for(txid);
    let mut identity = qualified(definition.schema.as_str(), definition.name.as_str())?;
    identity.write_char('(').map_err(|_| graph_full())?;
    let mut args = [EventAddressPart::new(); 2];
    let mut count = 0usize;
    for result in [definition.signature.left, definition.signature.right] {
        if count != 0 {
            identity.write_char(',').map_err(|_| graph_full())?;
        }
        let name = match result {
            Some(result) => routine_result_name(result)?,
            None => StackStr::from_str("NONE"),
        };
        identity
            .write_str(name.as_str())
            .map_err(|_| graph_full())?;
        args[count] = name;
        count += 1;
    }
    identity.write_char(')').map_err(|_| graph_full())?;
    let mut object = base_object(
        catalog::PG_OPERATOR_OID,
        operator.oid(),
        "operator",
        Some(definition.schema.as_str()),
        None,
        identity,
        false,
    );
    object.address_names[0] = StackStr::from_str(definition.schema.as_str());
    object.address_names[1] = StackStr::from_str(definition.name.as_str());
    object.address_name_count = 2;
    object.address_args[..count].copy_from_slice(&args[..count]);
    object.address_arg_count = count;
    Ok(object)
}

fn access_method_object(
    class_id: i32,
    oid: i32,
    object_type: &str,
    schema: &str,
    name: &str,
) -> Result<EventObject, SqlError> {
    let mut identity = qualified(schema, name)?;
    identity
        .write_str(" USING btree")
        .map_err(|_| graph_full())?;
    let mut object = base_object(
        class_id,
        oid,
        object_type,
        Some(schema),
        Some(name),
        identity,
        false,
    );
    object.address_args[0] = StackStr::from_str("btree");
    object.address_arg_count = 1;
    Ok(object)
}

fn primary_object(
    storage: &Storage,
    txid: u32,
    reference: ObjectRef,
) -> Result<EventObject, SqlError> {
    Ok(match reference {
        ObjectRef::Table(slot) => {
            let table = &storage.table(slot).def;
            base_object(
                catalog::PG_CLASS_OID,
                catalog::user_table_oid(slot),
                "table",
                Some(table.schema.as_str()),
                Some(table.name.as_str()),
                qualified(table.schema.as_str(), table.name.as_str())?,
                false,
            )
        }
        ObjectRef::View(slot) => {
            let view = storage.view(slot);
            base_object(
                catalog::PG_CLASS_OID,
                catalog::view_oid(slot),
                "view",
                Some(view.schema.as_str()),
                Some(view.name.as_str()),
                qualified(view.schema.as_str(), view.name.as_str())?,
                false,
            )
        }
        ObjectRef::Routine(slot) => routine_object(storage, txid, slot)?,
        ObjectRef::Sequence(slot) => {
            let sequence = storage.sequence_for(slot, txid);
            base_object(
                catalog::PG_CLASS_OID,
                catalog::sequence_oid(slot),
                "sequence",
                Some(sequence.schema.as_str()),
                Some(sequence.name.as_str()),
                qualified(sequence.schema.as_str(), sequence.name.as_str())?,
                false,
            )
        }
        ObjectRef::Domain(slot) => {
            let domain = storage.domain_for(slot, txid);
            base_object(
                catalog::PG_TYPE_OID,
                catalog::domain_oid(slot),
                "type",
                Some(domain.schema.as_str()),
                Some(domain.name.as_str()),
                qualified(domain.schema.as_str(), domain.name.as_str())?,
                true,
            )
        }
        ObjectRef::Enum(slot) => {
            let enumeration = storage.enum_for(slot, txid);
            base_object(
                catalog::PG_TYPE_OID,
                crate::sql::types::oid::enum_oid(slot as u16),
                "type",
                Some(enumeration.schema.as_str()),
                Some(enumeration.name.as_str()),
                qualified(enumeration.schema.as_str(), enumeration.name.as_str())?,
                true,
            )
        }
        ObjectRef::Composite(slot) => {
            let composite = storage.composite_for(slot, txid);
            base_object(
                catalog::PG_TYPE_OID,
                crate::sql::types::oid::composite_oid(slot as u16),
                "type",
                Some(composite.schema.as_str()),
                Some(composite.name.as_str()),
                qualified(composite.schema.as_str(), composite.name.as_str())?,
                true,
            )
        }
        ObjectRef::Index(slot) => {
            let index = storage
                .index_for_event_trigger(slot, txid)
                .ok_or_else(graph_full)?;
            let name = index.name_for(txid);
            base_object(
                catalog::PG_CLASS_OID,
                catalog::explicit_index_oid(&index),
                "index",
                Some(index.schema.as_str()),
                Some(name.as_str()),
                qualified(index.schema.as_str(), name.as_str())?,
                false,
            )
        }
        ObjectRef::Schema(slot) => {
            let schema = storage.schema_def(slot).name;
            base_object(
                catalog::PG_NAMESPACE_OID,
                catalog::namespace_oid_for_slot(slot),
                "schema",
                None,
                Some(schema.as_str()),
                StackStr::from_str(identifier(schema.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::MaterializedView(slot) => {
            let view = storage.matview(slot);
            let table_slot = (0..storage.table_count())
                .find(|candidate| {
                    let table = &storage.table(*candidate).def;
                    table.schema == view.schema && table.name == view.name
                })
                .ok_or_else(graph_full)?;
            base_object(
                catalog::PG_CLASS_OID,
                catalog::user_table_oid(table_slot),
                "materialized view",
                Some(view.schema.as_str()),
                Some(view.name.as_str()),
                qualified(view.schema.as_str(), view.name.as_str())?,
                false,
            )
        }
        ObjectRef::EventTrigger(slot) => {
            let trigger = storage.event_trigger(slot).definition_for(txid);
            base_object(
                3466,
                storage.event_trigger(slot).oid(),
                "event trigger",
                None,
                Some(trigger.name.as_str()),
                StackStr::from_str(identifier(trigger.name.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::Trigger(slot) => {
            let trigger = storage.trigger(slot);
            let (schema, table) = match trigger.target {
                crate::storage::TriggerTarget::Table(table) => {
                    let table = &storage.table(usize::from(table)).def;
                    (table.schema, table.name)
                }
                crate::storage::TriggerTarget::View(view) => {
                    let view = storage.view(usize::from(view));
                    (view.schema, view.name)
                }
            };
            let name = trigger.name_to(txid);
            let mut identity = EventIdentity::new();
            write!(
                identity,
                "{} on {}",
                identifier(name.as_str()).as_str(),
                qualified(schema.as_str(), table.as_str())?.as_str()
            )
            .map_err(|_| graph_full())?;
            let mut object = base_object(
                catalog::PG_TRIGGER_OID,
                crate::storage::trigger_oid(trigger),
                "trigger",
                Some(schema.as_str()),
                Some(name.as_str()),
                identity,
                false,
            );
            object.address_names[0] = StackStr::from_str(schema.as_str());
            object.address_names[1] = StackStr::from_str(table.as_str());
            object.address_names[2] = StackStr::from_str(name.as_str());
            object.address_name_count = 3;
            object
        }
        ObjectRef::Rule(slot) => {
            let rule = storage.rule(slot);
            let definition = rule.definition_for(txid);
            let (schema, relation) = match definition.target {
                crate::storage::RuleTarget::Table(table) => {
                    let table = storage.table_def(usize::from(table), txid);
                    (table.schema, table.name)
                }
                crate::storage::RuleTarget::View(view) => {
                    let view = storage.view(usize::from(view));
                    (view.schema, view.name)
                }
            };
            let mut identity = EventIdentity::new();
            write!(
                identity,
                "{} on {}",
                identifier(definition.name.as_str()).as_str(),
                qualified(schema.as_str(), relation.as_str())?.as_str()
            )
            .map_err(|_| graph_full())?;
            let mut object = base_object(
                catalog::PG_REWRITE_OID,
                rule.oid(),
                "rule",
                Some(schema.as_str()),
                Some(definition.name.as_str()),
                identity,
                false,
            );
            object.address_names[0] = StackStr::from_str(schema.as_str());
            object.address_names[1] = StackStr::from_str(relation.as_str());
            object.address_names[2] = StackStr::from_str(definition.name.as_str());
            object.address_name_count = 3;
            object
        }
        ObjectRef::Cast(slot) => {
            let cast = storage.cast(slot);
            let source = routine_result_name(cast.source)?;
            let target = routine_result_name(cast.target)?;
            let mut identity = EventIdentity::new();
            write!(identity, "({} AS {})", source.as_str(), target.as_str())
                .map_err(|_| graph_full())?;
            let mut object = base_object(
                catalog::PG_CAST_OID,
                cast.oid(),
                "cast",
                None,
                None,
                identity,
                false,
            );
            object.address_names[0] = source;
            object.address_names[1] = target;
            object.address_name_count = 2;
            object
        }
        ObjectRef::Operator(slot) => operator_object(storage, txid, slot)?,
        ObjectRef::OperatorFamily(slot) => {
            let family = storage.operator_family(slot);
            let definition = family.definition_for(txid);
            access_method_object(
                catalog::PG_OPFAMILY_OID,
                family.oid(),
                "operator family",
                definition.schema.as_str(),
                definition.name.as_str(),
            )?
        }
        ObjectRef::OperatorClass(slot) => {
            let class = storage.operator_class(slot);
            let definition = class.definition_for(txid);
            access_method_object(
                catalog::PG_OPCLASS_OID,
                class.oid(),
                "operator class",
                definition.schema.as_str(),
                definition.name.as_str(),
            )?
        }
        ObjectRef::Collation(slot) => {
            let collation = storage.collation(slot);
            let definition = collation.definition_for(txid);
            base_object(
                catalog::PG_COLLATION_OID,
                collation.oid(slot),
                "collation",
                Some(definition.schema.as_str()),
                Some(definition.name.as_str()),
                qualified(definition.schema.as_str(), definition.name.as_str())?,
                false,
            )
        }
        ObjectRef::Conversion(slot) => {
            let conversion = storage.conversion(slot);
            let definition = conversion.definition_for(txid);
            base_object(
                catalog::PG_CONVERSION_OID,
                conversion.oid(slot),
                "conversion",
                Some(definition.schema.as_str()),
                Some(definition.name.as_str()),
                qualified(definition.schema.as_str(), definition.name.as_str())?,
                false,
            )
        }
        ObjectRef::Policy(slot) => {
            let policy = storage.policy(slot);
            let table = storage.table_def(usize::from(policy.table), txid);
            let mut identity = EventIdentity::new();
            write!(
                identity,
                "{} on {}",
                identifier(policy.name.as_str()).as_str(),
                qualified(table.schema.as_str(), table.name.as_str())?.as_str()
            )
            .map_err(|_| graph_full())?;
            let mut object = base_object(
                catalog::PG_POLICY_OID,
                crate::storage::policy_oid(policy),
                "policy",
                Some(table.schema.as_str()),
                Some(policy.name.as_str()),
                identity,
                false,
            );
            object.address_names[0] = StackStr::from_str(table.schema.as_str());
            object.address_names[1] = StackStr::from_str(table.name.as_str());
            object.address_names[2] = StackStr::from_str(policy.name.as_str());
            object.address_name_count = 3;
            object
        }
        ObjectRef::Statistics(slot) => {
            let statistics = storage.extended_statistics(slot);
            let definition = statistics.definition_for(txid);
            base_object(
                catalog::PG_STATISTIC_EXT_OID,
                catalog::extended_statistics_oid(slot),
                "statistics object",
                Some(definition.schema.as_str()),
                Some(definition.name.as_str()),
                qualified(definition.schema.as_str(), definition.name.as_str())?,
                false,
            )
        }
        ObjectRef::Publication(slot) => {
            let publication = storage.publication_for_event_trigger(slot);
            let name = publication.name_for(txid);
            base_object(
                catalog::PG_PUBLICATION_OID,
                catalog::publication_oid(slot),
                "publication",
                None,
                Some(name.as_str()),
                StackStr::from_str(identifier(name.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::Subscription(slot) => {
            let subscription = storage.subscription_for_event_trigger(slot);
            let name = subscription.name_for(txid);
            base_object(
                catalog::PG_SUBSCRIPTION_OID,
                111_384 + i32::try_from(subscription.created_at).map_err(|_| graph_full())?,
                "subscription",
                None,
                Some(name.as_str()),
                StackStr::from_str(identifier(name.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::Extension(slot) => {
            let extension = storage.extension(slot);
            base_object(
                catalog::PG_EXTENSION_OID,
                catalog::extension_oid(slot),
                "extension",
                None,
                Some(extension.name.as_str()),
                StackStr::from_str(identifier(extension.name.as_str()).as_str()),
                false,
            )
        }
    })
}

fn explicit_table_drop(statement: &Stmt<'_>, object: &EventObject) -> bool {
    let Stmt::DropTable(DropTable { names, .. }) = statement else {
        return false;
    };
    names.iter().any(|name| {
        name.name
            == object
                .object_name
                .as_ref()
                .map_or("", |value| value.as_str())
            && Some(name.schema.unwrap_or("public"))
                == object.schema_name.as_ref().map(|value| value.as_str())
    })
}

fn qualified_name_matches(name: crate::sql::ast::QualName<'_>, object: &EventObject) -> bool {
    name.name
        == object
            .object_name
            .as_ref()
            .map_or("", |value| value.as_str())
        && Some(name.schema.unwrap_or("public"))
            == object.schema_name.as_ref().map(|value| value.as_str())
}

fn routine_identity_matches(
    storage: &Storage,
    txid: u32,
    slot: usize,
    identity: crate::sql::ast::RoutineIdentity<'_>,
) -> Result<bool, SqlError> {
    let routine = storage.routine_for(slot, txid);
    if identity.name.name != routine.name_for(txid).as_str()
        || identity.name.schema.unwrap_or("public") != routine.schema_for(txid).as_str()
    {
        return Ok(false);
    }
    if !identity.signature_is_explicit {
        return Ok(true);
    }
    if routine.arguments().len() != identity.argument_types.len() {
        return Ok(false);
    }
    let resolved =
        crate::sql::exec::resolve_routine_signature(storage, txid, identity.argument_types)?;
    Ok(routine
        .arguments()
        .iter()
        .zip(resolved)
        .all(|(stored, written)| {
            stored.ctype == written.ctype && stored.user_type == written.user_type
        }))
}

fn aggregate_identity_matches(
    storage: &Storage,
    txid: u32,
    slot: usize,
    identity: crate::sql::ast::AggregateIdentity<'_>,
) -> Result<bool, SqlError> {
    let mut names = [""; crate::storage::MAX_ROUTINE_ARGUMENTS];
    let count = identity.direct_argument_types.len() + identity.aggregated_argument_types.len();
    if count > names.len() {
        return Ok(false);
    }
    names[..identity.direct_argument_types.len()].copy_from_slice(identity.direct_argument_types);
    names[identity.direct_argument_types.len()..count]
        .copy_from_slice(identity.aggregated_argument_types);
    routine_identity_matches(
        storage,
        txid,
        slot,
        crate::sql::ast::RoutineIdentity {
            name: identity.name,
            argument_types: &names[..count],
            signature_is_explicit: true,
        },
    )
}

fn operator_identity_matches(
    storage: &Storage,
    txid: u32,
    slot: usize,
    identity: crate::sql::ast::OperatorIdentity<'_>,
) -> Result<bool, SqlError> {
    let definition = storage.operator(slot).definition_for(txid);
    if identity.name.name != definition.name.as_str()
        || identity.name.schema.unwrap_or("public") != definition.schema.as_str()
    {
        return Ok(false);
    }
    let signature = match identity.operands {
        crate::sql::ast::OperatorOperands::Prefix(right) => crate::storage::OperatorSignature {
            left: None,
            right: Some(crate::sql::exec::resolve_routine_type(
                storage, txid, right,
            )?),
        },
        crate::sql::ast::OperatorOperands::Binary { left, right } => {
            crate::storage::OperatorSignature {
                left: Some(crate::sql::exec::resolve_routine_type(storage, txid, left)?),
                right: Some(crate::sql::exec::resolve_routine_type(
                    storage, txid, right,
                )?),
            }
        }
    };
    Ok(signature == definition.signature)
}

fn is_original(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    reference: ObjectRef,
    object: &EventObject,
) -> Result<bool, SqlError> {
    Ok(match (statement, reference) {
        (Stmt::DropTable(_), ObjectRef::Table(_)) => explicit_table_drop(statement, object),
        (Stmt::DropView { names, .. }, ObjectRef::View(_))
        | (Stmt::DropSequence { names, .. }, ObjectRef::Sequence(_))
        | (Stmt::DropDomain { names, .. }, ObjectRef::Domain(_))
        | (Stmt::DropType { names, .. }, ObjectRef::Enum(_) | ObjectRef::Composite(_))
        | (Stmt::DropIndex { names, .. }, ObjectRef::Index(_)) => names.iter().any(|name| {
            name.name
                == object
                    .object_name
                    .as_ref()
                    .map_or("", |value| value.as_str())
                && Some(name.schema.unwrap_or("public"))
                    == object.schema_name.as_ref().map(|value| value.as_str())
        }),
        (Stmt::DropSchema { names, .. }, ObjectRef::Schema(_)) => names.iter().any(|name| {
            *name
                == object
                    .object_name
                    .as_ref()
                    .map_or("", |value| value.as_str())
        }),
        (Stmt::DropMaterializedView { names, .. }, ObjectRef::MaterializedView(_)) => names
            .iter()
            .any(|name| qualified_name_matches(*name, object)),
        (Stmt::DropFunction { functions, .. }, ObjectRef::Routine(slot))
        | (
            Stmt::DropProcedure {
                procedures: functions,
                ..
            },
            ObjectRef::Routine(slot),
        )
        | (
            Stmt::DropRoutine {
                routines: functions,
                ..
            },
            ObjectRef::Routine(slot),
        ) => functions.iter().try_fold(false, |matched, identity| {
            if matched {
                Ok(true)
            } else {
                routine_identity_matches(storage, txid, slot, *identity)
            }
        })?,
        (Stmt::DropAggregate { aggregates, .. }, ObjectRef::Routine(slot)) => {
            aggregates.iter().try_fold(false, |matched, identity| {
                if matched {
                    Ok(true)
                } else {
                    aggregate_identity_matches(storage, txid, slot, *identity)
                }
            })?
        }
        (Stmt::DropCast(_), ObjectRef::Cast(_)) => true,
        (Stmt::DropOperator { identities, .. }, ObjectRef::Operator(slot)) => {
            identities.iter().try_fold(false, |matched, identity| {
                if matched {
                    Ok(true)
                } else {
                    operator_identity_matches(storage, txid, slot, *identity)
                }
            })?
        }
        (Stmt::DropOperatorFamily { names, .. }, ObjectRef::OperatorFamily(_))
        | (Stmt::DropOperatorClass { names, .. }, ObjectRef::OperatorClass(_))
        | (Stmt::DropStatistics { names, .. }, ObjectRef::Statistics(_)) => names
            .iter()
            .any(|name| qualified_name_matches(*name, object)),
        (Stmt::DropCollation { name, .. }, ObjectRef::Collation(_))
        | (Stmt::DropConversion { name, .. }, ObjectRef::Conversion(_)) => {
            qualified_name_matches(*name, object)
        }
        (Stmt::DropPolicy { policy, .. }, ObjectRef::Policy(_)) => {
            policy.name == object.object_name.as_ref().map_or("", |name| name.as_str())
                && policy.table.name == object.address_names[1].as_str()
        }
        (Stmt::DropPublication { names, .. }, ObjectRef::Publication(_))
        | (Stmt::DropSubscription { names, .. }, ObjectRef::Subscription(_))
        | (Stmt::DropExtension { names, .. }, ObjectRef::Extension(_)) => {
            names.iter().any(|name| {
                *name
                    == object
                        .object_name
                        .as_ref()
                        .map_or("", |value| value.as_str())
            })
        }
        (Stmt::DropTrigger { trigger, .. }, ObjectRef::Trigger(_)) => {
            trigger.name == object.object_name.as_ref().map_or("", |name| name.as_str())
                && trigger.table.name == object.address_names[1].as_str()
        }
        (Stmt::DropRule(rule), ObjectRef::Rule(_)) => {
            rule.name == object.object_name.as_ref().map_or("", |name| name.as_str())
                && rule.table.name == object.address_names[1].as_str()
        }
        _ => false,
    })
}

fn push_command(
    output: &mut [DdlCommand; MAX_EVENT_OBJECTS],
    count: &mut usize,
    reference: EventObjectRef,
    tag: &str,
    in_extension: bool,
) -> Result<(), SqlError> {
    let slot = output.get_mut(*count).ok_or_else(graph_full)?;
    *slot = DdlCommand {
        reference,
        command_tag: StackStr::from_str(tag),
        in_extension,
    };
    *count += 1;
    Ok(())
}

fn push_drop(
    output: &mut [DroppedObject; MAX_EVENT_OBJECTS],
    count: &mut usize,
    reference: EventObjectRef,
    original: bool,
    normal: bool,
) -> Result<(), SqlError> {
    let slot = output.get_mut(*count).ok_or_else(graph_full)?;
    *slot = DroppedObject {
        reference,
        original,
        normal,
        temporary: false,
    };
    *count += 1;
    Ok(())
}

fn push_drop_once(
    output: &mut [DroppedObject; MAX_EVENT_OBJECTS],
    count: &mut usize,
    reference: EventObjectRef,
    original: bool,
    normal: bool,
) -> Result<(), SqlError> {
    if output[..*count]
        .iter()
        .any(|dropped| dropped.reference == reference)
    {
        return Ok(());
    }
    push_drop(output, count, reference, original, normal)
}

fn detached_constraint(
    table: &crate::storage::TableDef,
    oid: i32,
    name: impl AsRef<str>,
) -> EventObjectRef {
    EventObjectRef::DetachedConstraint {
        oid,
        schema: table.schema,
        table: table.name,
        name: StackStr::from_str(name.as_ref()),
    }
}

fn detached_index(
    table: &crate::storage::TableDef,
    oid: i32,
    name: impl AsRef<str>,
) -> EventObjectRef {
    EventObjectRef::DetachedIndex {
        oid,
        schema: table.schema,
        name: StackStr::from_str(name.as_ref()),
    }
}

fn foreign_key_trigger_reference(
    child: &crate::storage::TableDef,
    child_slot: usize,
    foreign_key_index: usize,
    foreign_key: &crate::storage::ForeignKey,
    ordinal: usize,
) -> EventObjectRef {
    let oid = catalog::foreign_key_trigger_oid(child_slot, foreign_key_index, ordinal);
    let side = if ordinal < 2 { "a" } else { "c" };
    EventObjectRef::DetachedTrigger {
        oid,
        schema: if ordinal < 2 {
            foreign_key.parent_schema
        } else {
            child.schema
        },
        table: if ordinal < 2 {
            foreign_key.parent
        } else {
            child.name
        },
        name: crate::stack_format!(128, "RI_ConstraintTrigger_{}_{}", side, oid),
    }
}

fn push_foreign_key_trigger_drops(
    child: &crate::storage::TableDef,
    child_slot: usize,
    foreign_key_index: usize,
    foreign_key: &crate::storage::ForeignKey,
    output: &mut [DroppedObject; MAX_EVENT_OBJECTS],
    count: &mut usize,
) -> Result<(), SqlError> {
    for ordinal in 0..4 {
        push_drop_once(
            output,
            count,
            foreign_key_trigger_reference(
                child,
                child_slot,
                foreign_key_index,
                foreign_key,
                ordinal,
            ),
            false,
            false,
        )?;
    }
    Ok(())
}

fn table_index_positions(
    table: &crate::storage::TableDef,
) -> ([Option<u16>; crate::storage::MAX_COLUMNS], usize) {
    let mut inline = [None; crate::storage::MAX_COLUMNS];
    let mut position = 0usize;
    for (column, output) in table.columns().iter().zip(&mut inline) {
        if column.primary || column.unique {
            *output = Some(position as u16);
            position += 1;
        }
    }
    (inline, position)
}

fn push_alter_table_drops(
    statement: &Stmt<'_>,
    before: BeforeDdl<'_>,
    storage: &Storage,
    txid: u32,
    output: &mut [DroppedObject; MAX_EVENT_OBJECTS],
    count: &mut usize,
) -> Result<(), SqlError> {
    let (slot, old) = match before.altered_table {
        Some(state) => state,
        None => return Ok(()),
    };
    let Stmt::AlterTable(alter) = statement else {
        return Ok(());
    };
    let new = storage.table_def(slot, txid);
    let table_oid = catalog::user_table_oid(slot);
    let (inline_positions, named_start) = table_index_positions(old);

    let constraint_removed = |name: &str| {
        !new.checks().iter().any(|item| item.name.as_str() == name)
            && !new.fkeys().iter().any(|item| item.name.as_str() == name)
            && !new.uniques().iter().any(|item| item.name.as_str() == name)
            && !new
                .exclusions()
                .iter()
                .any(|item| item.name.as_str() == name)
            && !new.columns().iter().any(|column| {
                (column.primary && inline_primary_name(new).as_str() == name)
                    || (column.unique && inline_unique_name(new, column).as_str() == name)
                    || (column.not_null.is_required()
                        && not_null_name(new, column).as_str() == name)
            })
    };

    for action in alter.actions {
        match action {
            crate::sql::ast::AlterAction::DropColumn { name, .. } => {
                let Some(column_index) = old.column_index(name) else {
                    continue;
                };
                let column = old.columns()[column_index];
                push_drop_once(
                    output,
                    count,
                    EventObjectRef::DetachedColumn {
                        table_oid,
                        schema: old.schema,
                        table: old.name,
                        column: column.name,
                        attnum: u16::try_from(column_index + 1).map_err(|_| graph_full())?,
                    },
                    true,
                    false,
                )?;
            }
            crate::sql::ast::AlterAction::DropNotNull { column } => {
                let Some(column_index) = old.column_index(column) else {
                    continue;
                };
                let metadata = old.columns()[column_index];
                if metadata.not_null.is_required() {
                    let name = not_null_name(old, &metadata);
                    push_drop_once(
                        output,
                        count,
                        detached_constraint(
                            old,
                            catalog::FIRST_NOT_NULL_OID
                                + slot as i32 * crate::storage::MAX_COLUMNS as i32
                                + column_index as i32,
                            name.as_str(),
                        ),
                        true,
                        false,
                    )?;
                }
            }
            crate::sql::ast::AlterAction::DropConstraint { name, .. } => {
                if let Some(index) = old
                    .checks()
                    .iter()
                    .position(|constraint| constraint.name.as_str() == *name)
                {
                    push_drop_once(
                        output,
                        count,
                        detached_constraint(
                            old,
                            catalog::FIRST_CHECK_OID
                                + slot as i32 * crate::storage::MAX_CHECKS as i32
                                + index as i32,
                            *name,
                        ),
                        true,
                        false,
                    )?;
                } else if let Some(index) = old
                    .fkeys()
                    .iter()
                    .position(|constraint| constraint.name.as_str() == *name)
                {
                    push_drop_once(
                        output,
                        count,
                        detached_constraint(
                            old,
                            catalog::FIRST_FK_OID + slot as i32 * 64 + index as i32,
                            *name,
                        ),
                        true,
                        false,
                    )?;
                } else if let Some(index) = old
                    .uniques()
                    .iter()
                    .position(|constraint| constraint.name.as_str() == *name)
                {
                    let index_oid = catalog::index_oid(slot, named_start + index);
                    push_drop_once(
                        output,
                        count,
                        detached_constraint(old, index_oid + 500_000, *name),
                        true,
                        false,
                    )?;
                    push_drop_once(
                        output,
                        count,
                        detached_index(old, index_oid, *name),
                        false,
                        false,
                    )?;
                } else if let Some(index) = old
                    .exclusions()
                    .iter()
                    .position(|constraint| constraint.name.as_str() == *name)
                {
                    let index_oid =
                        catalog::index_oid(slot, named_start + old.uniques().len() + index);
                    push_drop_once(
                        output,
                        count,
                        detached_constraint(old, index_oid + 500_000, *name),
                        true,
                        false,
                    )?;
                    push_drop_once(
                        output,
                        count,
                        detached_index(old, index_oid, *name),
                        false,
                        false,
                    )?;
                } else {
                    for (column_index, column) in old.columns().iter().enumerate() {
                        let generated = if column.primary {
                            Some(inline_primary_name(old))
                        } else if column.unique {
                            Some(inline_unique_name(old, column))
                        } else if column.not_null.is_required() {
                            Some(not_null_name(old, column))
                        } else {
                            None
                        };
                        if generated
                            .as_ref()
                            .is_some_and(|generated| generated.as_str() == *name)
                        {
                            if column.primary || column.unique {
                                let index_oid = catalog::index_oid(
                                    slot,
                                    usize::from(
                                        inline_positions[column_index].ok_or_else(graph_full)?,
                                    ),
                                );
                                push_drop_once(
                                    output,
                                    count,
                                    detached_constraint(old, index_oid + 500_000, *name),
                                    true,
                                    false,
                                )?;
                                push_drop_once(
                                    output,
                                    count,
                                    detached_index(old, index_oid, *name),
                                    false,
                                    false,
                                )?;
                            } else {
                                push_drop_once(
                                    output,
                                    count,
                                    detached_constraint(
                                        old,
                                        catalog::FIRST_NOT_NULL_OID
                                            + slot as i32 * crate::storage::MAX_COLUMNS as i32
                                            + column_index as i32,
                                        *name,
                                    ),
                                    true,
                                    false,
                                )?;
                            }
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    for (index, check) in old.checks().iter().enumerate() {
        if constraint_removed(check.name.as_str()) {
            push_drop_once(
                output,
                count,
                detached_constraint(
                    old,
                    catalog::FIRST_CHECK_OID
                        + slot as i32 * crate::storage::MAX_CHECKS as i32
                        + index as i32,
                    check.name.as_str(),
                ),
                false,
                true,
            )?;
        }
    }
    for (index, foreign_key) in old.fkeys().iter().enumerate() {
        if constraint_removed(foreign_key.name.as_str()) {
            push_drop_once(
                output,
                count,
                detached_constraint(
                    old,
                    catalog::FIRST_FK_OID + slot as i32 * 64 + index as i32,
                    foreign_key.name.as_str(),
                ),
                false,
                true,
            )?;
            push_foreign_key_trigger_drops(old, slot, index, foreign_key, output, count)?;
        }
    }
    for (column_index, column) in old.columns().iter().enumerate() {
        let survives = new
            .columns()
            .iter()
            .find(|new_column| new_column.name == column.name);
        if column.not_null.is_required()
            && survives.is_none_or(|new_column| !new_column.not_null.is_required())
        {
            let name = not_null_name(old, column);
            push_drop_once(
                output,
                count,
                detached_constraint(
                    old,
                    catalog::FIRST_NOT_NULL_OID
                        + slot as i32 * crate::storage::MAX_COLUMNS as i32
                        + column_index as i32,
                    name.as_str(),
                ),
                false,
                false,
            )?;
        }
        if (column.primary || column.unique)
            && survives.is_none_or(|new_column| {
                (column.primary && !new_column.primary) || (column.unique && !new_column.unique)
            })
        {
            let name = if column.primary {
                inline_primary_name(old)
            } else {
                inline_unique_name(old, column)
            };
            let index_oid = catalog::index_oid(
                slot,
                usize::from(inline_positions[column_index].ok_or_else(graph_full)?),
            );
            push_drop_once(
                output,
                count,
                detached_constraint(old, index_oid + 500_000, name.as_str()),
                false,
                false,
            )?;
            push_drop_once(
                output,
                count,
                detached_index(old, index_oid, name.as_str()),
                false,
                false,
            )?;
        }
    }
    for (index, unique) in old.uniques().iter().enumerate() {
        if !new
            .uniques()
            .iter()
            .any(|candidate| candidate.name == unique.name)
        {
            let index_oid = catalog::index_oid(slot, named_start + index);
            push_drop_once(
                output,
                count,
                detached_constraint(old, index_oid + 500_000, unique.name.as_str()),
                false,
                false,
            )?;
            push_drop_once(
                output,
                count,
                detached_index(old, index_oid, unique.name.as_str()),
                false,
                false,
            )?;
        }
    }
    for (index, exclusion) in old.exclusions().iter().enumerate() {
        if !new
            .exclusions()
            .iter()
            .any(|candidate| candidate.name == exclusion.name)
        {
            let index_oid = catalog::index_oid(slot, named_start + old.uniques().len() + index);
            push_drop_once(
                output,
                count,
                detached_constraint(old, index_oid + 500_000, exclusion.name.as_str()),
                false,
                false,
            )?;
            push_drop_once(
                output,
                count,
                detached_index(old, index_oid, exclusion.name.as_str()),
                false,
                false,
            )?;
        }
    }
    Ok(())
}

fn push_table_indexes(
    storage: &Storage,
    txid: u32,
    slot: usize,
    output: &mut [DdlCommand; MAX_EVENT_OBJECTS],
    count: &mut usize,
    in_extension: bool,
) -> Result<(), SqlError> {
    let table = storage.table_def(slot, txid);
    let mut position = 0usize;
    for column in table.columns() {
        let name = if column.primary {
            Some(inline_primary_name(table))
        } else if column.unique {
            Some(inline_unique_name(table, column))
        } else {
            None
        };
        if let Some(name) = name {
            push_command(
                output,
                count,
                EventObjectRef::TableIndex {
                    table: u16::try_from(slot).map_err(|_| graph_full())?,
                    position: u16::try_from(position).map_err(|_| graph_full())?,
                    name,
                },
                "CREATE INDEX",
                in_extension,
            )?;
            position += 1;
        }
    }
    for unique in table.uniques() {
        push_command(
            output,
            count,
            EventObjectRef::TableIndex {
                table: u16::try_from(slot).map_err(|_| graph_full())?,
                position: u16::try_from(position).map_err(|_| graph_full())?,
                name: StackStr::from_str(unique.name.as_str()),
            },
            "CREATE INDEX",
            in_extension,
        )?;
        position += 1;
    }
    for exclusion in table.exclusions() {
        push_command(
            output,
            count,
            EventObjectRef::TableIndex {
                table: u16::try_from(slot).map_err(|_| graph_full())?,
                position: u16::try_from(position).map_err(|_| graph_full())?,
                name: StackStr::from_str(exclusion.name.as_str()),
            },
            "CREATE INDEX",
            in_extension,
        )?;
        position += 1;
    }
    Ok(())
}

fn push_table_drop_dependents(
    storage: &Storage,
    txid: u32,
    slot: usize,
    output: &mut [DroppedObject; MAX_EVENT_OBJECTS],
    count: &mut usize,
) -> Result<(), SqlError> {
    let table = storage.table_def(slot, txid);
    let table_ref = u16::try_from(slot).map_err(|_| graph_full())?;
    push_drop(
        output,
        count,
        EventObjectRef::TableRowType(table_ref),
        false,
        false,
    )?;
    push_drop(
        output,
        count,
        EventObjectRef::TableArrayType(table_ref),
        false,
        false,
    )?;

    for (index, check) in table.checks().iter().enumerate() {
        push_drop(
            output,
            count,
            EventObjectRef::TableConstraint {
                table: table_ref,
                oid: catalog::FIRST_CHECK_OID
                    + slot as i32 * crate::storage::MAX_CHECKS as i32
                    + index as i32,
                name: StackStr::from_str(check.name.as_str()),
            },
            false,
            true,
        )?;
    }
    for (index, column) in table.columns().iter().enumerate() {
        if column.not_null.is_required() {
            let name = not_null_name(table, column);
            push_drop(
                output,
                count,
                EventObjectRef::TableConstraint {
                    table: table_ref,
                    oid: catalog::FIRST_NOT_NULL_OID
                        + slot as i32 * crate::storage::MAX_COLUMNS as i32
                        + index as i32,
                    name,
                },
                false,
                false,
            )?;
        }
    }
    for (index, foreign_key) in table.fkeys().iter().enumerate() {
        push_drop(
            output,
            count,
            EventObjectRef::TableConstraint {
                table: table_ref,
                oid: catalog::FIRST_FK_OID + slot as i32 * 64 + index as i32,
                name: StackStr::from_str(foreign_key.name.as_str()),
            },
            false,
            true,
        )?;
        push_foreign_key_trigger_drops(table, slot, index, foreign_key, output, count)?;
    }
    if table.has_toast {
        push_drop(
            output,
            count,
            EventObjectRef::ToastRelation(table_ref),
            false,
            false,
        )?;
        push_drop(
            output,
            count,
            EventObjectRef::ToastIndex(table_ref),
            false,
            false,
        )?;
    }

    let mut position = 0usize;
    for column in table.columns() {
        let name = if column.primary {
            Some(inline_primary_name(table))
        } else if column.unique {
            Some(inline_unique_name(table, column))
        } else {
            None
        };
        if let Some(name) = name {
            let index_oid = catalog::index_oid(slot, position);
            push_drop(
                output,
                count,
                EventObjectRef::TableConstraint {
                    table: table_ref,
                    oid: index_oid + 500_000,
                    name,
                },
                false,
                false,
            )?;
            push_drop(
                output,
                count,
                EventObjectRef::TableIndex {
                    table: table_ref,
                    position: u16::try_from(position).map_err(|_| graph_full())?,
                    name,
                },
                false,
                false,
            )?;
            position += 1;
        }
    }
    for unique in table.uniques() {
        let index_oid = catalog::index_oid(slot, position);
        push_drop(
            output,
            count,
            EventObjectRef::TableConstraint {
                table: table_ref,
                oid: index_oid + 500_000,
                name: StackStr::from_str(unique.name.as_str()),
            },
            false,
            false,
        )?;
        push_drop(
            output,
            count,
            EventObjectRef::TableIndex {
                table: table_ref,
                position: u16::try_from(position).map_err(|_| graph_full())?,
                name: StackStr::from_str(unique.name.as_str()),
            },
            false,
            false,
        )?;
        position += 1;
    }
    for exclusion in table.exclusions() {
        let index_oid = catalog::index_oid(slot, position);
        push_drop(
            output,
            count,
            EventObjectRef::TableConstraint {
                table: table_ref,
                oid: index_oid + 500_000,
                name: StackStr::from_str(exclusion.name.as_str()),
            },
            false,
            false,
        )?;
        push_drop(
            output,
            count,
            EventObjectRef::TableIndex {
                table: table_ref,
                position: u16::try_from(position).map_err(|_| graph_full())?,
                name: StackStr::from_str(exclusion.name.as_str()),
            },
            false,
            false,
        )?;
        position += 1;
    }
    Ok(())
}

fn push_view_drop_dependents(
    _storage: &Storage,
    slot: usize,
    parent_is_normal: bool,
    output: &mut [DroppedObject; MAX_EVENT_OBJECTS],
    count: &mut usize,
) -> Result<(), SqlError> {
    let view_ref = u16::try_from(slot).map_err(|_| graph_full())?;
    push_drop(
        output,
        count,
        EventObjectRef::ViewRowType(view_ref),
        false,
        false,
    )?;
    push_drop(
        output,
        count,
        EventObjectRef::ViewArrayType(view_ref),
        false,
        false,
    )?;
    push_drop(
        output,
        count,
        EventObjectRef::ViewRule(view_ref),
        false,
        parent_is_normal,
    )
}

fn push_type_drop_dependents(
    storage: &Storage,
    txid: u32,
    reference: ObjectRef,
    output: &mut [DroppedObject; MAX_EVENT_OBJECTS],
    count: &mut usize,
) -> Result<(), SqlError> {
    match reference {
        ObjectRef::Domain(slot) => {
            let domain = storage.domain_for(slot, txid);
            let domain_ref = u16::try_from(slot).map_err(|_| graph_full())?;
            push_drop(
                output,
                count,
                EventObjectRef::DomainArray(domain_ref),
                false,
                false,
            )?;
            for index in 0..domain.checks().len() {
                push_drop(
                    output,
                    count,
                    EventObjectRef::DomainConstraint {
                        domain: domain_ref,
                        constraint: u16::try_from(index).map_err(|_| graph_full())?,
                    },
                    false,
                    false,
                )?;
            }
        }
        ObjectRef::Enum(slot) => {
            push_drop(
                output,
                count,
                EventObjectRef::EnumArray(u16::try_from(slot).map_err(|_| graph_full())?),
                false,
                false,
            )?;
        }
        ObjectRef::Composite(slot) => {
            let composite_ref = u16::try_from(slot).map_err(|_| graph_full())?;
            push_drop(
                output,
                count,
                EventObjectRef::CompositeRelation(composite_ref),
                false,
                false,
            )?;
            push_drop(
                output,
                count,
                EventObjectRef::CompositeArray(composite_ref),
                false,
                false,
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn push_drop_dependents(
    storage: &Storage,
    txid: u32,
    reference: ObjectRef,
    parent_is_normal: bool,
    output: &mut [DroppedObject; MAX_EVENT_OBJECTS],
    count: &mut usize,
) -> Result<(), SqlError> {
    match reference {
        ObjectRef::Table(slot) => push_table_drop_dependents(storage, txid, slot, output, count),
        ObjectRef::MaterializedView(slot) => {
            let view = storage.matview(slot);
            let table_slot = (0..storage.table_count())
                .find(|candidate| {
                    let table = &storage.table(*candidate).def;
                    table.schema == view.schema && table.name == view.name
                })
                .ok_or_else(graph_full)?;
            push_table_drop_dependents(storage, txid, table_slot, output, count)
        }
        ObjectRef::View(slot) => {
            push_view_drop_dependents(storage, slot, parent_is_normal, output, count)
        }
        ObjectRef::Domain(_) | ObjectRef::Enum(_) | ObjectRef::Composite(_) => {
            push_type_drop_dependents(storage, txid, reference, output, count)
        }
        _ => Ok(()),
    }
}

pub(crate) struct CollectChanges<'a, 'b> {
    pub before: BeforeDdl<'a>,
    pub undo: &'b [DdlUndo],
    pub undo_origins: &'b [u32],
    pub origin: u32,
    pub in_extension: bool,
}

pub(crate) struct EventGraphs<'a> {
    pub commands: &'a mut [DdlCommand; MAX_EVENT_OBJECTS],
    pub drops: &'a mut [DroppedObject; MAX_EVENT_OBJECTS],
}

pub(crate) fn collect(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    tag: &str,
    changes: CollectChanges<'_, '_>,
    graphs: EventGraphs<'_>,
) -> Result<(usize, usize), SqlError> {
    let CollectChanges {
        before,
        undo,
        undo_origins,
        origin,
        in_extension,
    } = changes;
    let EventGraphs { commands, drops } = graphs;
    let mut command_count = 0;
    let mut drop_count = 0;
    if let Some(object_type) = utility_command_object_type(statement) {
        push_command(
            commands,
            &mut command_count,
            EventObjectRef::Utility(StackStr::from_str(object_type)),
            tag,
            in_extension,
        )?;
    }
    for original_pass in [true, false] {
        for (&entry, &entry_origin) in undo.iter().zip(undo_origins) {
            if entry_origin != origin {
                continue;
            }
            let Some((reference, Mutation::Drop)) = mutation(entry) else {
                continue;
            };
            let object = primary_object(storage, txid, reference)?;
            let original = is_original(storage, txid, statement, reference, &object)?;
            if original != original_pass {
                continue;
            }
            push_drop(
                drops,
                &mut drop_count,
                EventObjectRef::Primary(reference),
                original,
                !original,
            )?;
            push_drop_dependents(storage, txid, reference, !original, drops, &mut drop_count)?;
        }
    }
    push_alter_table_drops(statement, before, storage, txid, drops, &mut drop_count)?;
    for (reference, normal) in before.dependent_drops[..before.dependent_drop_count]
        .iter()
        .flatten()
        .copied()
    {
        push_drop_once(drops, &mut drop_count, reference, false, normal)?;
    }
    let mut seen_commands: [Option<EventObjectRef>; crate::sql::txn::MAX_TXN_DDL] =
        [None; crate::sql::txn::MAX_TXN_DDL];
    let mut seen_command_count = 0usize;
    for (&entry, &entry_origin) in undo.iter().zip(undo_origins) {
        if entry_origin != origin {
            continue;
        }
        if let DdlUndo::CommentSet { slot, .. } = entry {
            let reference = comment_reference(storage, txid, slot as usize, statement)?;
            if seen_commands[..seen_command_count].contains(&Some(reference)) {
                continue;
            }
            seen_commands[seen_command_count] = Some(reference);
            seen_command_count += 1;
            push_command(commands, &mut command_count, reference, tag, in_extension)?;
            continue;
        }
        let Some((reference, mutation)) = mutation(entry) else {
            continue;
        };
        if mutation != Mutation::Drop {
            if let Stmt::AlterTable(_) = statement
                && let Some((root, _)) = before.altered_table
                && matches!(reference, ObjectRef::Table(slot) if slot != root)
            {
                continue;
            }
            let command_reference = EventObjectRef::Primary(reference);
            if seen_commands[..seen_command_count].contains(&Some(command_reference)) {
                continue;
            }
            seen_commands[seen_command_count] = Some(command_reference);
            seen_command_count += 1;
            push_command(
                commands,
                &mut command_count,
                command_reference,
                tag,
                in_extension,
            )?;
            if mutation == Mutation::Create
                && let ObjectRef::Table(slot) = reference
            {
                push_table_indexes(
                    storage,
                    txid,
                    slot,
                    commands,
                    &mut command_count,
                    in_extension,
                )?;
            }
        }
    }
    Ok((command_count, drop_count))
}
