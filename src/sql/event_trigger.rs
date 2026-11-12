//! Transaction-local object graphs exposed to PostgreSQL event-trigger helpers.

use core::cell::Cell;
use core::fmt::Write as _;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::Stmt;
use crate::sql::catalog;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::txn::DdlUndo;
use crate::sql_err;
use crate::storage::{RoutineKind, Storage};
use crate::util::StackStr;

pub(crate) const MAX_ADDRESS_PARTS: usize = crate::storage::MAX_DEFINITION_ITEMS;
type EventIdentity = StackStr<8192>;
type EventAddressPart = StackStr<512>;

pub(crate) struct BeforeDdl<'a> {
    altered_table: Option<(usize, crate::storage::TableDefinitionImages)>,
    // Keep the selected objects while their names and search-path entries exist.
    original_drops: &'a [ObjectRef],
    indexes: &'a [(usize, crate::storage::RelationIdentity)],
    dependent_drops: &'a [(EventObjectRef, bool)],
}

impl BeforeDdl<'_> {
    pub(crate) const EMPTY: Self = Self {
        altered_table: None,
        original_drops: &[],
        indexes: &[],
        dependent_drops: &[],
    };
}

pub(crate) fn capture_before<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    arena: &'a Arena,
) -> Result<BeforeDdl<'a>, SqlError> {
    let mut before = BeforeDdl::EMPTY;
    before.original_drops = capture_original_drops(storage, txid, statement, arena)?;
    let mut indexes = ArenaList::new(arena);
    for slot in 0..storage.index_count() {
        if let Some(index) = storage.index_visible_to(slot, txid) {
            indexes
                .push((
                    slot,
                    crate::storage::RelationIdentity::parse(index.created_at)
                        .expect("admitted index has a relation identity"),
                ))
                .map_err(|_| graph_full())?;
        }
    }
    before.indexes = indexes.as_slice();
    let mut dependent_drops = ArenaList::new(arena);
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
        let definitions = storage.table_definition_images();
        definitions.definition(storage, slot, txid)?;
        before.altered_table = Some((slot, definitions));
    }

    let table_is_explicitly_dropped = |slot: usize| {
        let table_schema = {
            let table = storage.table_def(slot, txid);
            table.schema
        };
        match statement {
            Stmt::DropTable(_) | Stmt::DropForeignTable(_) => {
                before.original_drops.contains(&ObjectRef::Table(slot))
            }
            Stmt::DropSchema { names, .. } => {
                names.iter().any(|schema| *schema == table_schema.as_str())
            }
            _ => false,
        }
    };

    for child_slot in 0..storage.table_count() {
        if table_is_explicitly_dropped(child_slot) {
            continue;
        }
        let definitions = storage.table_definition_images();
        let child = match before.altered_table.as_ref() {
            Some((slot, retained)) if *slot == child_slot => {
                retained.definition(storage, child_slot, txid)?
            }
            _ => definitions.definition(storage, child_slot, txid)?,
        };
        for foreign_key in child.fkeys() {
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
                        foreign_key_references_named_key(&parent, name, foreign_key.parent_cols())
                    }
                    _ => false,
                })
            } else {
                false
            };
            if !affected {
                continue;
            }
            let reference =
                detached_constraint(child, foreign_key.oid(), foreign_key.name.as_str());
            if dependent_drops
                .as_slice()
                .iter()
                .any(|(existing, _)| *existing == reference)
            {
                continue;
            }
            dependent_drops
                .push((reference, true))
                .map_err(|_| graph_full())?;
            for trigger in crate::storage::ForeignKeyTrigger::ALL {
                let reference = foreign_key_trigger_reference(child, foreign_key, trigger);
                dependent_drops
                    .push((reference, false))
                    .map_err(|_| graph_full())?;
            }
        }
    }
    before.dependent_drops = dependent_drops.as_slice();
    Ok(before)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventObject {
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
    reference: CommandReference,
    pub command_tag: StackStr<64>,
    pub in_extension: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "event objects stay inline in the bounded statement arena; heap boxing is forbidden"
)]
enum CommandReference {
    Object(EventObjectRef),
    ReindexedIndex {
        slot: usize,
        identity: crate::storage::RelationIdentity,
    },
}

impl DdlCommand {
    pub(crate) fn object(
        self,
        storage: &Storage,
        txid: u32,
    ) -> Result<Option<EventObject>, SqlError> {
        match self.reference {
            CommandReference::Object(reference) => reference.materialize(storage, txid).map(Some),
            CommandReference::ReindexedIndex { slot, identity } => {
                let Some(index) = storage.index_visible_to(slot, txid) else {
                    return Ok(None);
                };
                if index.created_at != identity.created_at() {
                    return Ok(None);
                }
                index_definition_object(&index, txid).map(Some)
            }
        }
    }

    pub(crate) const fn addressed(self) -> bool {
        !matches!(
            self.reference,
            CommandReference::Object(EventObjectRef::Utility(_))
        )
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
    TextSearch(usize),
    Policy(usize),
    Statistics(usize),
    Publication(usize),
    Subscription(usize),
    Extension(usize),
    LargeObject(usize),
    ForeignDataWrapper(usize),
    ForeignServer(usize),
    Role(usize),
    AccessMethod(i32),
    ProceduralLanguage(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventObjectRef {
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
    /// `chunk_id`, `chunk_seq` or `chunk_data` (attributes 1–3) of a table's
    /// TOAST relation.
    ToastColumn {
        table: u16,
        attnum: u16,
    },
    ToastIndex(u16),
    TableConstraint {
        table: u16,
        oid: i32,
        name: StackStr<128>,
    },
    ViewRowType(u16),
    ViewArrayType(u16),
    ViewRule(u16),
    MatviewRule(u16),
    DomainArray(u16),
    DomainConstraint {
        domain: u16,
        constraint: crate::storage::ConstraintIdentity,
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
                    table.implicit_index_oid(usize::from(position)),
                    name.as_str(),
                    &table,
                )?
            }
            Self::TableRowType(slot) => {
                let slot = usize::from(slot);
                let table = storage.table_def(slot, txid);
                base_object(
                    catalog::PG_TYPE_OID,
                    catalog::table_row_type_oid(storage, slot, false),
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
                    catalog::table_row_type_oid(storage, slot, true),
                )?
            }
            Self::ToastRelation(slot) | Self::ToastIndex(slot) => {
                let slot = usize::from(slot);
                let table_oid = catalog::user_table_oid(storage, slot);
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
                        catalog::toast_index_oid(storage, slot, txid)
                    } else {
                        catalog::toast_relation_oid(storage, slot)
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
                &storage.table_def(usize::from(table), txid),
            )?,
            Self::ViewRowType(slot) => {
                let slot = usize::from(slot);
                let view = storage.view(slot);
                base_object(
                    catalog::PG_TYPE_OID,
                    catalog::view_row_type_oid(storage, slot, false),
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
                    catalog::view_row_type_oid(storage, slot, true),
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
            Self::MatviewRule(slot) => {
                let matview = storage.matview(usize::from(slot));
                let relation = storage.table_def(usize::from(matview.backing_table), txid);
                let mut identity = EventIdentity::new();
                write!(
                    identity,
                    "\"_RETURN\" on {}",
                    qualified(relation.schema.as_str(), relation.name.as_str())?.as_str()
                )
                .map_err(|_| graph_full())?;
                let mut rule = base_object(
                    catalog::PG_REWRITE_OID,
                    matview.rule_oid(),
                    "rule",
                    None,
                    None,
                    identity,
                    false,
                );
                rule.address_names[0] = StackStr::from_str(relation.schema.as_str());
                rule.address_names[1] = StackStr::from_str(relation.name.as_str());
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
                    crate::sql::types::oid::domain_array_oid(
                        storage.domain_for(slot, txid).creation(),
                    ),
                )?
            }
            Self::DomainConstraint { domain, constraint } => {
                let domain_slot = usize::from(domain);
                let domain = storage.domain_for(domain_slot, txid);
                let name = domain.constraint_name(constraint).ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "domain constraint no longer exists"
                    )
                })?;
                let mut identity = EventIdentity::new();
                write!(
                    identity,
                    "{} on {}",
                    identifier(name.as_str()).as_str(),
                    qualified(domain.schema.as_str(), domain.name.as_str())?.as_str()
                )
                .map_err(|_| graph_full())?;
                let mut object = base_object(
                    catalog::PG_CONSTRAINT_OID,
                    constraint.oid(),
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
                object.address_args[0] = StackStr::from_str(name.as_str());
                object.address_arg_count = 1;
                object
            }
            Self::EnumArray(slot) => {
                let slot = usize::from(slot);
                let enumeration = storage.enum_for(slot, txid);
                array_type_object(
                    enumeration.schema.as_str(),
                    enumeration.name.as_str(),
                    crate::sql::types::oid::enum_array_oid(storage.enum_for(slot, txid).creation()),
                )?
            }
            Self::CompositeRelation(slot) => {
                let slot = usize::from(slot);
                let composite = storage.composite_for(slot, txid);
                base_object(
                    catalog::PG_CLASS_OID,
                    catalog::named_composite_relation_oid(storage, slot),
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
                    crate::sql::types::oid::composite_array_oid(
                        storage.composite_for(slot, txid).creation(),
                    ),
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
            Self::ToastColumn { table, attnum } => {
                let mut object = Self::ToastRelation(table).materialize(storage, txid)?;
                let relation_name = object.object_name.ok_or_else(graph_full)?;
                let name = TOAST_COLUMNS[usize::from(attnum) - 1];
                let mut identity = qualified("pg_toast", relation_name.as_str())?;
                write!(identity, ".{name}").map_err(|_| graph_full())?;
                object.sub_id = i32::from(attnum);
                object.object_type = StackStr::from_str("toast table column");
                object.object_name = None;
                object.identity = identity;
                object.address_names = [StackStr::new(); MAX_ADDRESS_PARTS];
                object.address_names[0] = StackStr::from_str("pg_toast");
                object.address_names[1] = StackStr::from_str(relation_name.as_str());
                object.address_names[2] = StackStr::from_str(name);
                object.address_name_count = 3;
                object
            }
            Self::RelationColumn {
                relation,
                attnum,
                name,
            } => {
                let mut object = primary_object(storage, txid, relation)?;
                if let ObjectRef::Composite(slot) = relation {
                    // A composite type's columns belong to its relation.
                    object.class_id = catalog::PG_CLASS_OID;
                    object.object_id = catalog::named_composite_relation_oid(storage, slot);
                }
                let relation_name = object.object_name.ok_or_else(graph_full)?;
                let schema = object.schema_name.ok_or_else(graph_full)?;
                let mut identity = qualified(schema.as_str(), relation_name.as_str())?;
                write!(identity, ".{}", identifier(name.as_str()).as_str())
                    .map_err(|_| graph_full())?;
                object.sub_id = i32::from(attnum);
                object.object_type = StackStr::from_str(match relation {
                    ObjectRef::View(_) => "view column",
                    ObjectRef::MaterializedView(_) => "materialized view column",
                    ObjectRef::Composite(_) => "composite type column",
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
                let detached = crate::storage::TableDefView {
                    schema,
                    name: table,
                    ..crate::storage::TableDefView::empty()
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

fn mutation(storage: &Storage, undo: DdlUndo) -> Option<(ObjectRef, Mutation)> {
    use DdlUndo::*;
    Some(match undo {
        Created(slot) => (ObjectRef::Table(slot as usize), Mutation::Create),
        Dropped(undo) => (ObjectRef::Table(undo.slot()), Mutation::Drop),
        TableAltered(change) => (ObjectRef::Table(change.table()), Mutation::Alter),
        ViewChanged(crate::storage::ViewPublication::Created(undo)) => {
            (ObjectRef::View(undo.slot()), Mutation::Create)
        }
        ViewDropped(undo) => (ObjectRef::View(undo.slot()), Mutation::Drop),
        ViewChanged(crate::storage::ViewPublication::Altered(undo)) => {
            (ObjectRef::View(undo.slot()), Mutation::Alter)
        }
        RoutineChanged(change) => (
            ObjectRef::Routine(change.slot()),
            if change.is_creation() {
                Mutation::Create
            } else if change.is_drop() {
                Mutation::Drop
            } else {
                Mutation::Alter
            },
        ),
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
        TextSearchCreated(slot) => (ObjectRef::TextSearch(slot as usize), Mutation::Create),
        TextSearchAltered { slot, .. } => (ObjectRef::TextSearch(slot as usize), Mutation::Alter),
        TextSearchDropped(slot) => (ObjectRef::TextSearch(slot as usize), Mutation::Drop),
        EventTriggerCreated(slot) => (ObjectRef::EventTrigger(slot as usize), Mutation::Create),
        EventTriggerDropped(slot) => (ObjectRef::EventTrigger(slot as usize), Mutation::Drop),
        EventTriggerAltered { slot, .. } => {
            (ObjectRef::EventTrigger(slot as usize), Mutation::Alter)
        }
        TriggerCreated(slot) => (ObjectRef::Trigger(slot as usize), Mutation::Create),
        TriggerDropped(slot) => (ObjectRef::Trigger(slot as usize), Mutation::Drop),
        TriggerAltered { slot, .. } => (ObjectRef::Trigger(slot as usize), Mutation::Alter),
        RuleChanged { publication, .. } => (
            ObjectRef::Rule(publication.slot()),
            match publication {
                crate::storage::RulePublication::Created(_) => Mutation::Create,
                crate::storage::RulePublication::Altered(_) => Mutation::Alter,
            },
        ),
        RuleDropped(undo) => (ObjectRef::Rule(undo.slot()), Mutation::Drop),
        RuleAltered(undo) => (ObjectRef::Rule(undo.slot()), Mutation::Alter),
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
        MatviewChanged(change) => (
            ObjectRef::MaterializedView(change.slot()),
            if change.is_creation() {
                Mutation::Create
            } else {
                Mutation::Drop
            },
        ),
        MatviewPopulationChanged(undo) => {
            (ObjectRef::MaterializedView(undo.slot()), Mutation::Alter)
        }
        SequenceChanged(change) => (
            ObjectRef::Sequence(change.slot()),
            if change.is_creation() {
                Mutation::Create
            } else if change.is_drop() {
                Mutation::Drop
            } else {
                Mutation::Alter
            },
        ),
        DomainCreated(undo) => (ObjectRef::Domain(undo.slot()), Mutation::Create),
        DomainDropped(undo) => (ObjectRef::Domain(undo.slot()), Mutation::Drop),
        DomainAltered(undo) => (ObjectRef::Domain(undo.slot()), Mutation::Alter),
        EnumCreated(undo) => (ObjectRef::Enum(undo.slot()), Mutation::Create),
        EnumDropped(undo) => (ObjectRef::Enum(undo.slot()), Mutation::Drop),
        EnumAltered(undo) => (ObjectRef::Enum(undo.slot()), Mutation::Alter),
        CompositeCreated(undo) => (ObjectRef::Composite(undo.slot()), Mutation::Create),
        CompositeDropped(undo) => (ObjectRef::Composite(undo.slot()), Mutation::Drop),
        CompositeAltered(undo) => (ObjectRef::Composite(undo.slot()), Mutation::Alter),
        IndexCreated(slot) => (ObjectRef::Index(slot as usize), Mutation::Create),
        IndexDropped(slot) => (ObjectRef::Index(slot as usize), Mutation::Drop),
        IndexRenamed { slot, .. } | IndexAltered { slot, .. } => {
            (ObjectRef::Index(slot as usize), Mutation::Alter)
        }
        AccessMethodCreated(slot) => (
            ObjectRef::AccessMethod(storage.access_method_oid_at(slot as usize)?),
            Mutation::Create,
        ),
        AccessMethodDropped(slot) => (
            ObjectRef::AccessMethod(storage.access_method_oid_at(slot as usize)?),
            Mutation::Drop,
        ),
        SchemaCreated(slot) => (ObjectRef::Schema(slot as usize), Mutation::Create),
        SchemaDropped(slot) => (ObjectRef::Schema(slot as usize), Mutation::Drop),
        ExtensionCreated(slot) => (ObjectRef::Extension(slot as usize), Mutation::Create),
        ExtensionDropped(slot) => (ObjectRef::Extension(slot as usize), Mutation::Drop),
        ExtensionAltered { slot, .. } => (ObjectRef::Extension(slot as usize), Mutation::Alter),
        ObjectOwnerChanged { object, .. } => (access_reference(object)?, Mutation::Alter),
        _ => return None,
    })
}

/// PostgreSQL's `quote_identifier`, which also quotes reserved words.
fn identifier(value: &str) -> StackStr<132> {
    crate::sql::eval::quote_identifier(value)
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
        "event-trigger object graph exceeds statement memory"
    )
}

fn utility_command_object_type(statement: &Stmt<'_>) -> Option<&'static str> {
    use crate::sql::ast::{DefaultPrivilegeObjectKind, PrivilegeObjectKind, PrivilegeTarget};
    let privilege_type = |target: PrivilegeTarget<'_>| {
        Some(match target {
            PrivilegeTarget::Parameters(_) => "PARAMETER",
            PrivilegeTarget::Objects { kind, .. } => match kind {
                PrivilegeObjectKind::Table | PrivilegeObjectKind::AllTablesInSchema => "TABLE",
                PrivilegeObjectKind::Sequence | PrivilegeObjectKind::AllSequencesInSchema => {
                    "SEQUENCE"
                }
                PrivilegeObjectKind::Schema => "SCHEMA",
                PrivilegeObjectKind::Type => "TYPE",
                PrivilegeObjectKind::ProceduralLanguage => "LANGUAGE",
                PrivilegeObjectKind::AllFunctionsInSchema => "FUNCTION",
                PrivilegeObjectKind::AllProceduresInSchema => "PROCEDURE",
                PrivilegeObjectKind::AllRoutinesInSchema => "ROUTINE",
                PrivilegeObjectKind::ForeignDataWrapper => "FOREIGN DATA WRAPPER",
                PrivilegeObjectKind::ForeignServer => "FOREIGN SERVER",
                PrivilegeObjectKind::Tablespace | PrivilegeObjectKind::Database => return None,
            },
            PrivilegeTarget::Routines { kind, .. } => match kind {
                crate::sql::ast::RoutineTargetKind::Function => "FUNCTION",
                crate::sql::ast::RoutineTargetKind::Procedure => "PROCEDURE",
                crate::sql::ast::RoutineTargetKind::Aggregate => "AGGREGATE",
                crate::sql::ast::RoutineTargetKind::Either => "ROUTINE",
            },
            PrivilegeTarget::LargeObjects(_) => "LARGE OBJECT",
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
        AccessClass::LargeObject => ObjectRef::LargeObject(slot),
        AccessClass::Language | AccessClass::BuiltinRoutine => return None,
        AccessClass::Tablespace
        | AccessClass::Database
        | AccessClass::EventTrigger
        | AccessClass::ForeignDataWrapper
        | AccessClass::ForeignServer => return None,
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
                    ObjectRef::Table(table) => {
                        let table = storage.table_def(table, txid);
                        u16::try_from(subid)
                            .ok()
                            .and_then(|number| table.attribute_column(number))
                            .map(|column| table.columns()[column].name)
                    }
                    ObjectRef::MaterializedView(view_slot) => {
                        let table = storage.table_def(storage.matview_table(view_slot), txid);
                        u16::try_from(subid)
                            .ok()
                            .and_then(|number| table.attribute_column(number))
                            .map(|column| table.columns()[column].name)
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
                        ObjectRef::MaterializedView(slot) => EventObjectRef::TableRowType(
                            u16::try_from(storage.matview_table(slot)).map_err(|_| graph_full())?,
                        ),
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
        CommentClass::TextSearchParser
        | CommentClass::TextSearchTemplate
        | CommentClass::TextSearchDictionary
        | CommentClass::TextSearchConfiguration => {
            let kind = match class {
                CommentClass::TextSearchParser => crate::sql::ast::TextSearchObjectKind::Parser,
                CommentClass::TextSearchTemplate => crate::sql::ast::TextSearchObjectKind::Template,
                CommentClass::TextSearchDictionary => {
                    crate::sql::ast::TextSearchObjectKind::Dictionary
                }
                CommentClass::TextSearchConfiguration => {
                    crate::sql::ast::TextSearchObjectKind::Configuration
                }
                _ => unreachable!("text-search comment class guard"),
            };
            EventObjectRef::Primary(ObjectRef::TextSearch(
                storage
                    .text_search_slot(kind, schema.as_str(), name.as_str(), txid)
                    .ok_or_else(graph_full)?,
            ))
        }
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
        CommentClass::Cast => {
            let oid = i32::try_from(subid).map_err(|_| graph_full())?;
            EventObjectRef::Primary(ObjectRef::Cast(
                storage
                    .casts_visible_to(txid)
                    .find_map(|(slot, cast)| (cast.oid() == oid).then_some(slot))
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::Operator => {
            let oid = i32::try_from(subid).map_err(|_| graph_full())?;
            EventObjectRef::Primary(ObjectRef::Operator(
                storage
                    .operator_slot_by_oid(oid, txid)
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::OperatorFamily => {
            let oid = i32::try_from(subid).map_err(|_| graph_full())?;
            EventObjectRef::Primary(ObjectRef::OperatorFamily(
                storage
                    .operator_family_slot_by_oid(oid, txid)
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::OperatorClass => {
            let oid = crate::storage::OperatorClassOid::parse(
                i32::try_from(subid).map_err(|_| graph_full())?,
            )
            .ok_or_else(graph_full)?;
            EventObjectRef::Primary(ObjectRef::OperatorClass(
                storage
                    .operator_class_slot_by_oid(oid, txid)
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::Constraint => {
            let table = usize::try_from(subid).map_err(|_| graph_full())?;
            let oid = catalog::table_constraint_oid(storage, txid, table, name.as_str())
                .ok_or_else(graph_full)?;
            EventObjectRef::TableConstraint {
                table: u16::try_from(table).map_err(|_| graph_full())?,
                oid,
                name: StackStr::from_str(name.as_str()),
            }
        }
        CommentClass::Role => EventObjectRef::Primary(ObjectRef::Role(
            storage
                .role_slot_by_oid(i32::try_from(subid).map_err(|_| graph_full())?, txid)
                .ok_or_else(graph_full)?,
        )),
        CommentClass::ForeignServer => EventObjectRef::Primary(ObjectRef::ForeignServer(
            storage
                .foreign_servers(txid)
                .find_map(|(slot, entry)| (entry.creation().get() == subid).then_some(slot))
                .ok_or_else(graph_full)?,
        )),
        CommentClass::ForeignDataWrapper => EventObjectRef::Primary(ObjectRef::ForeignDataWrapper(
            storage
                .foreign_wrappers(txid)
                .find_map(|(slot, entry)| (entry.creation().get() == subid).then_some(slot))
                .ok_or_else(graph_full)?,
        )),
        CommentClass::AccessMethod => {
            let oid = i32::try_from(subid).map_err(|_| graph_full())?;
            catalog::access_method_name_in(storage, txid, oid)
                .is_some_and(|candidate| candidate.as_str() == name.as_str())
                .then_some(EventObjectRef::Primary(ObjectRef::AccessMethod(oid)))
                .ok_or_else(graph_full)?
        }
        CommentClass::ProceduralLanguage => {
            let oid = i32::try_from(subid).map_err(|_| graph_full())?;
            (catalog::procedural_language_name(oid) == Some(name.as_str()))
                .then_some(EventObjectRef::Primary(ObjectRef::ProceduralLanguage(oid)))
                .ok_or_else(graph_full)?
        }
        CommentClass::LargeObject => {
            let oid = name
                .as_str()
                .parse::<u32>()
                .ok()
                .and_then(crate::sql::ast::LargeObjectId::parse)
                .ok_or_else(graph_full)?;
            EventObjectRef::Primary(ObjectRef::LargeObject(
                storage
                    .large_object_slot(oid, txid)
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::Routine => {
            let oid = i32::try_from(subid).map_err(|_| graph_full())?;
            EventObjectRef::Primary(ObjectRef::Routine(
                storage
                    .routine_slot_by_oid(oid, txid)
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::Policy => {
            let oid = i32::try_from(subid).map_err(|_| graph_full())?;
            EventObjectRef::Primary(ObjectRef::Policy(
                storage
                    .policies_with_slots_visible_to(txid)
                    .find_map(|(slot, policy)| {
                        (crate::storage::policy_oid(&policy) == oid).then_some(slot)
                    })
                    .ok_or_else(graph_full)?,
            ))
        }
        CommentClass::Statistics => EventObjectRef::Primary(ObjectRef::Statistics(
            storage
                .extended_statistics_visible(txid)
                .find_map(|(slot, statistics)| {
                    (statistics.creation().get() == subid).then_some(slot)
                })
                .ok_or_else(graph_full)?,
        )),
        CommentClass::Publication => EventObjectRef::Primary(ObjectRef::Publication(
            storage
                .publications_with_slots_visible_to(txid)
                .find_map(|(slot, publication)| {
                    (publication.creation().get() == subid).then_some(slot)
                })
                .ok_or_else(graph_full)?,
        )),
        CommentClass::Subscription => EventObjectRef::Primary(ObjectRef::Subscription(
            storage
                .subscriptions_with_slots_visible_to(Some(storage.current_database_oid()), txid)
                .find_map(|(slot, subscription)| {
                    (subscription.created_at as u32 == subid).then_some(slot)
                })
                .ok_or_else(graph_full)?,
        )),
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

fn constraint_object<Definition0: crate::storage::DefinitionBacking>(
    oid: i32,
    name: &str,
    table: &crate::storage::TableDefinition<Definition0>,
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

fn index_object<Definition0: crate::storage::DefinitionBacking>(
    oid: i32,
    name: &str,
    table: &crate::storage::TableDefinition<Definition0>,
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

fn foreign_key_references_named_key<Definition0: crate::storage::DefinitionBacking>(
    table: &crate::storage::TableDefinition<Definition0>,
    name: &str,
    parent_columns: &[u16],
) -> bool {
    if let Some(key) = table.uniques().iter().find(|key| key.name.as_str() == name) {
        return key.columns() == parent_columns;
    }
    for (index, column) in table.columns().iter().enumerate() {
        let generated = (column.primary || column.unique).then(|| {
            table
                .column_key_name(index)
                .expect("admitted inline key has a name")
        });
        if generated
            .as_ref()
            .is_some_and(|generated| generated.as_str() == name)
        {
            return parent_columns == [index as u16];
        }
    }
    false
}

fn not_null_name<Definition0: crate::storage::DefinitionBacking>(
    table: &crate::storage::TableDefinition<Definition0>,
    column: &crate::storage::ColumnMeta,
) -> StackStr<128> {
    StackStr::from_str(
        table.not_null_names.as_ref()[table
            .column_index(column.name.as_str())
            .expect("constraint column exists")]
        .expect("required column has a durable constraint name")
        .as_str(),
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
    qualified_type_display(display, catalog_name)
}

/// `format_type_be_qualified` of a built-in type OID: `pg_catalog.text`,
/// `pg_catalog."any"`, `integer`, `pg_catalog.cstring[]`.
fn builtin_type_identity_name(type_oid: i32) -> Result<EventAddressPart, SqlError> {
    let display = catalog::intrinsic_type_name(type_oid).ok_or_else(graph_full)?;
    let rows = catalog::builtin_type_rows();
    let row = |oid: i32| {
        rows.binary_search_by_key(&oid, |row| row.oid)
            .ok()
            .map(|index| &rows[index])
    };
    let mut base = row(type_oid).ok_or_else(graph_full)?;
    if display.ends_with("[]") {
        base = row(base.element).ok_or_else(graph_full)?;
    }
    qualified_type_display(display, base.name)
}

/// Types PostgreSQL spells with SQL syntax (`integer`, `character varying`)
/// stay unqualified; every other type name is schema-qualified. `json` is
/// printed bare, as the SQL/JSON standard names it.
fn qualified_type_display(display: &str, catalog_name: &str) -> Result<EventAddressPart, SqlError> {
    let quoted = crate::sql::eval::quote_identifier(catalog_name);
    let name = if display.trim_end_matches("[]") == quoted.as_str() && catalog_name != "json" {
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

fn index_definition_object(
    index: &crate::storage::IndexDef,
    txid: u32,
) -> Result<EventObject, SqlError> {
    let name = index.name_for(txid);
    Ok(base_object(
        catalog::PG_CLASS_OID,
        catalog::explicit_index_oid(index),
        "index",
        Some(index.schema.as_str()),
        Some(name.as_str()),
        qualified(index.schema.as_str(), name.as_str())?,
        false,
    ))
}

fn access_method_object(
    class_id: i32,
    oid: i32,
    object_type: &str,
    schema: &str,
    name: &str,
) -> Result<EventObject, SqlError> {
    access_method_object_using(class_id, oid, object_type, schema, name, "btree")
}

/// A built-in operator class or family, which lives in `pg_catalog`.
fn builtin_access_method_object(
    class_id: i32,
    oid: i32,
    method: i32,
    name: &str,
) -> Result<EventObject, SqlError> {
    let method = catalog::access_method_name(method).ok_or_else(graph_full)?;
    let object_type = if class_id == catalog::PG_OPCLASS_OID {
        "operator class"
    } else {
        "operator family"
    };
    access_method_object_using(class_id, oid, object_type, "pg_catalog", name, method)
}

/// PostgreSQL's identity of a built-in cast: both types as
/// `format_type_be_qualified` prints them.
fn builtin_cast_object(oid: i32, source: i32, target: i32) -> Result<EventObject, SqlError> {
    let qualified_type = |type_oid: i32| -> Result<EventAddressPart, SqlError> {
        let name = catalog::intrinsic_type_name(type_oid).ok_or_else(graph_full)?;
        // format_type prints these SQL-standard spellings without a schema.
        const SQL_NAMES: &[&str] = &[
            "bit",
            "boolean",
            "character",
            "real",
            "double precision",
            "smallint",
            "integer",
            "bigint",
            "numeric",
            "interval",
            "time without time zone",
            "time with time zone",
            "timestamp without time zone",
            "timestamp with time zone",
            "bit varying",
            "character varying",
            "json",
        ];
        let name = if SQL_NAMES.contains(&name) {
            StackStr::from_str(name)
        } else {
            crate::stack_format!(512, "pg_catalog.{name}")
        };
        (!name.is_truncated())
            .then_some(name)
            .ok_or_else(graph_full)
    };
    let source = qualified_type(source)?;
    let target = qualified_type(target)?;
    let mut identity = EventIdentity::new();
    write!(identity, "({} AS {})", source.as_str(), target.as_str()).map_err(|_| graph_full())?;
    let mut object = base_object(
        catalog::PG_CAST_OID,
        oid,
        "cast",
        None,
        None,
        identity,
        false,
    );
    object.address_names[0] = source;
    object.address_names[1] = target;
    object.address_name_count = 2;
    Ok(object)
}

fn access_method_object_using(
    class_id: i32,
    oid: i32,
    object_type: &str,
    schema: &str,
    name: &str,
    method: &str,
) -> Result<EventObject, SqlError> {
    let mut identity = qualified(schema, name)?;
    write!(identity, " USING {method}").map_err(|_| graph_full())?;
    let mut object = base_object(
        class_id,
        oid,
        object_type,
        Some(schema),
        Some(name),
        identity,
        false,
    );
    object.address_args[0] = StackStr::from_str(method);
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
            let table = storage.table_def(slot, txid);
            base_object(
                catalog::PG_CLASS_OID,
                catalog::user_table_oid(storage, slot),
                if table.kind == crate::storage::TableKind::Foreign {
                    "foreign table"
                } else {
                    "table"
                },
                Some(table.schema.as_str()),
                Some(table.name.as_str()),
                qualified(table.schema.as_str(), table.name.as_str())?,
                false,
            )
        }
        ObjectRef::View(slot) => {
            let view = storage.view(slot);
            let schema = view.schema_for(txid);
            let name = view.name_for(txid);
            base_object(
                catalog::PG_CLASS_OID,
                catalog::view_oid(storage, slot),
                "view",
                Some(schema.as_str()),
                Some(name.as_str()),
                qualified(schema.as_str(), name.as_str())?,
                false,
            )
        }
        ObjectRef::Routine(slot) => routine_object(storage, txid, slot)?,
        ObjectRef::Sequence(slot) => {
            let sequence = storage.sequence_for(slot, txid);
            base_object(
                catalog::PG_CLASS_OID,
                catalog::sequence_oid(storage, slot),
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
                catalog::domain_oid(storage, slot, txid),
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
                crate::sql::types::oid::enum_oid(storage.enum_for(slot, txid).creation()),
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
                crate::sql::types::oid::composite_oid(storage.composite_for(slot, txid).creation()),
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
            index_definition_object(&index, txid)?
        }
        ObjectRef::Schema(slot) => {
            let schema = storage.schema_def(slot).name;
            base_object(
                catalog::PG_NAMESPACE_OID,
                catalog::namespace_oid_for_slot(storage, slot),
                "schema",
                None,
                Some(schema.as_str()),
                StackStr::from_str(identifier(schema.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::MaterializedView(slot) => {
            let table_slot = storage.matview_table(slot);
            let table = storage.table_def(table_slot, txid);
            base_object(
                catalog::PG_CLASS_OID,
                catalog::user_table_oid(storage, table_slot),
                "materialized view",
                Some(table.schema.as_str()),
                Some(table.name.as_str()),
                qualified(table.schema.as_str(), table.name.as_str())?,
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
                    let table = &*storage.table(usize::from(table)).definition();
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
                crate::storage::trigger_oid(&trigger),
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
                collation.oid(),
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
                conversion.oid(),
                "conversion",
                Some(definition.schema.as_str()),
                Some(definition.name.as_str()),
                qualified(definition.schema.as_str(), definition.name.as_str())?,
                false,
            )
        }
        ObjectRef::TextSearch(slot) => {
            let definition = storage.text_search_object(slot).definition_for(txid);
            let (classoid, object_type) = match definition.kind() {
                crate::sql::ast::TextSearchObjectKind::Parser => {
                    (catalog::PG_TS_PARSER_OID, "text search parser")
                }
                crate::sql::ast::TextSearchObjectKind::Template => {
                    (catalog::PG_TS_TEMPLATE_OID, "text search template")
                }
                crate::sql::ast::TextSearchObjectKind::Dictionary => {
                    (catalog::PG_TS_DICT_OID, "text search dictionary")
                }
                crate::sql::ast::TextSearchObjectKind::Configuration => {
                    (catalog::PG_TS_CONFIG_OID, "text search configuration")
                }
            };
            base_object(
                classoid,
                definition.oid(),
                object_type,
                Some(definition.schema().as_str()),
                Some(definition.name().as_str()),
                qualified(definition.schema().as_str(), definition.name().as_str())?,
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
                crate::storage::policy_oid(&policy),
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
                statistics.oid(),
                "statistics object",
                Some(definition.schema.as_str()),
                Some(definition.name.as_str()),
                qualified(definition.schema.as_str(), definition.name.as_str())?,
                false,
            )
        }
        ObjectRef::Publication(slot) => {
            let publication = storage.publication_at(slot);
            let name = publication.name_for(txid);
            base_object(
                catalog::PG_PUBLICATION_OID,
                publication.oid(),
                "publication",
                None,
                Some(name.as_str()),
                StackStr::from_str(identifier(name.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::Subscription(slot) => {
            let subscription = storage.subscription_at(slot);
            let name = subscription.name_for(txid);
            base_object(
                catalog::PG_SUBSCRIPTION_OID,
                subscription.oid(),
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
                extension.oid(),
                "extension",
                None,
                Some(extension.name.as_str()),
                StackStr::from_str(identifier(extension.name.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::LargeObject(slot) => {
            let oid = storage.large_object(slot).oid.get();
            let identity = StackStr::from_str(crate::stack_format!(16, "{}", oid).as_str());
            base_object(
                catalog::PG_LARGEOBJECT_OID,
                oid as i32,
                "large object",
                None,
                None,
                identity,
                false,
            )
        }
        ObjectRef::ForeignServer(slot) => {
            let server = storage
                .foreign_server_by_slot(slot, txid)
                .ok_or_else(graph_full)?;
            base_object(
                catalog::PG_FOREIGN_SERVER_OID,
                storage.foreign_server_entry(slot).oid(),
                "server",
                None,
                Some(server.name.as_str()),
                StackStr::from_str(identifier(server.name.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::ForeignDataWrapper(slot) => {
            let wrapper = storage
                .foreign_wrapper_by_slot(slot, txid)
                .ok_or_else(graph_full)?;
            base_object(
                catalog::PG_FOREIGN_DATA_WRAPPER_OID,
                storage.foreign_wrapper_entry(slot).oid(),
                "foreign-data wrapper",
                None,
                Some(wrapper.name.as_str()),
                StackStr::from_str(identifier(wrapper.name.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::Role(slot) => {
            let role = storage.role(slot);
            base_object(
                1260,
                storage.role_oid(slot),
                "role",
                None,
                Some(role.name_to(txid).as_str()),
                StackStr::from_str(identifier(role.name_to(txid).as_str()).as_str()),
                false,
            )
        }
        ObjectRef::AccessMethod(oid) => {
            let name = match catalog::access_method_name(oid) {
                Some(name) => crate::storage::SqlName::parse(name).map_err(|_| graph_full())?,
                None => storage
                    .access_method_name_by_oid(oid)
                    .ok_or_else(graph_full)?,
            };
            base_object(
                catalog::PG_AM_OID,
                oid,
                "access method",
                None,
                Some(name.as_str()),
                StackStr::from_str(identifier(name.as_str()).as_str()),
                false,
            )
        }
        ObjectRef::ProceduralLanguage(oid) => {
            let name = catalog::procedural_language_name(oid).ok_or_else(graph_full)?;
            base_object(
                catalog::PG_LANGUAGE_OID,
                oid,
                "language",
                None,
                Some(name),
                StackStr::from_str(identifier(name).as_str()),
                false,
            )
        }
    })
}

fn catalog_lookup_failed(class_id: i32, object_id: i32) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_OBJECT,
        "cache lookup failed for object {} of catalog {}",
        object_id,
        class_id
    )
}

fn address_candidate(
    storage: &Storage,
    txid: u32,
    reference: EventObjectRef,
    object_type: &str,
    names: &[&str],
    arguments: &[&str],
) -> Result<Option<(i32, i32, i32)>, SqlError> {
    let object = reference.materialize(storage, txid)?;
    let matches = object.object_type.as_str() == object_type
        && object.address_name_count == names.len()
        && object.address_arg_count == arguments.len()
        && object.address_names[..object.address_name_count]
            .iter()
            .zip(names)
            .all(|(stored, requested)| stored.as_str() == *requested)
        && object.address_args[..object.address_arg_count]
            .iter()
            .zip(arguments)
            .all(|(stored, requested)| stored.as_str() == *requested);
    Ok(matches.then_some((object.class_id, object.object_id, object.sub_id)))
}

/// Resolve the unquoted address components emitted by
/// `pg_identify_object_as_address`. Candidate objects are materialized through
/// the event-trigger graph before comparison, making the two APIs exact
/// inverses without a second identity formatter.
pub(crate) fn catalog_object_address(
    storage: &Storage,
    txid: u32,
    object_type: &str,
    names: &[&str],
    arguments: &[&str],
) -> Result<(i32, i32, i32), SqlError> {
    if !matches!(
        object_type,
        "table"
            | "table column"
            | "view"
            | "view column"
            | "materialized view"
            | "materialized view column"
            | "sequence"
            | "index"
            | "composite type"
            | "type"
            | "domain constraint"
            | "table constraint"
            | "function"
            | "procedure"
            | "aggregate"
            | "schema"
            | "operator"
            | "operator family"
            | "operator class"
            | "collation"
            | "conversion"
            | "statistics object"
            | "text search parser"
            | "text search template"
            | "text search dictionary"
            | "text search configuration"
            | "event trigger"
            | "trigger"
            | "rule"
            | "cast"
            | "policy"
            | "publication"
            | "subscription"
            | "server"
            | "foreign-data wrapper"
            | "extension"
            | "large object"
            | "role"
            | "access method"
            | "language"
    ) {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "unrecognized object type \"{}\"",
            object_type
        ));
    }
    macro_rules! consider {
        ($reference:expr) => {
            if let Some(address) =
                address_candidate(storage, txid, $reference, object_type, names, arguments)?
            {
                return Ok(address);
            }
        };
    }

    for slot in 0..storage.schema_count() {
        if storage.schema_def(slot).visible_to(txid) {
            consider!(EventObjectRef::Primary(ObjectRef::Schema(slot)));
        }
    }
    for (oid, name) in [(11, "pg_catalog"), (99, "pg_toast")] {
        let object = base_object(
            catalog::PG_NAMESPACE_OID,
            oid,
            "schema",
            None,
            Some(name),
            StackStr::from_str(name),
            false,
        );
        if object.object_type.as_str() == object_type && names == [name] && arguments.is_empty() {
            return Ok((object.class_id, object.object_id, object.sub_id));
        }
    }

    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let relation = relation_object_ref(storage, txid, catalog::user_table_oid(storage, slot))
            .expect("visible tables have relation objects");
        consider!(relation);
        let table = storage.table_def(slot, txid);
        for (column, definition) in table.columns().iter().enumerate() {
            consider!(EventObjectRef::RelationColumn {
                relation: match relation {
                    EventObjectRef::Primary(reference) => reference,
                    _ => unreachable!("relation object is primary"),
                },
                attnum: table.attribute_number(column),
                name: definition.name,
            });
        }
        consider!(EventObjectRef::TableRowType(slot as u16));
        consider!(EventObjectRef::TableArrayType(slot as u16));
        if names.len() == 3
            && names[0] == table.schema.as_str()
            && names[1] == table.name.as_str()
            && arguments.is_empty()
            && let Some(oid) = catalog::table_constraint_oid(storage, txid, slot, names[2])
        {
            consider!(EventObjectRef::TableConstraint {
                table: slot as u16,
                oid,
                name: StackStr::from_str(names[2]),
            });
        }
    }
    for slot in 0..storage.view_count() {
        if !storage.view_slot_visible_to(slot, txid) {
            continue;
        }
        let relation = ObjectRef::View(slot);
        consider!(EventObjectRef::Primary(relation));
        let view = storage.view(slot);
        for (column, definition) in view.columns.names().iter().enumerate() {
            consider!(EventObjectRef::RelationColumn {
                relation,
                attnum: (column + 1) as u16,
                name: *definition,
            });
        }
        consider!(EventObjectRef::ViewRowType(slot as u16));
        consider!(EventObjectRef::ViewArrayType(slot as u16));
    }
    for slot in 0..storage.sequence_count() {
        if storage.sequence_slot_visible_to(slot, txid) {
            consider!(EventObjectRef::Primary(ObjectRef::Sequence(slot)));
        }
    }
    for slot in 0..storage.index_count() {
        if storage.index_visible_to(slot, txid).is_some() {
            consider!(EventObjectRef::Primary(ObjectRef::Index(slot)));
        }
    }
    for slot in 0..storage.domain_count() {
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        consider!(EventObjectRef::Primary(ObjectRef::Domain(slot)));
        consider!(EventObjectRef::DomainArray(slot as u16));
        for constraint in storage.domain_for(slot, txid).constraint_identities() {
            consider!(EventObjectRef::DomainConstraint {
                domain: slot as u16,
                constraint: constraint.expect("admitted domain constraint has an identity"),
            });
        }
    }
    for slot in 0..storage.enum_count() {
        if storage.enum_slot_visible_to(slot, txid) {
            consider!(EventObjectRef::Primary(ObjectRef::Enum(slot)));
            consider!(EventObjectRef::EnumArray(slot as u16));
        }
    }
    for (slot, _) in storage.composites_with_slots_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Composite(slot)));
        consider!(EventObjectRef::CompositeRelation(slot as u16));
        consider!(EventObjectRef::CompositeArray(slot as u16));
    }
    for slot in 0..storage.routine_count() {
        if storage.routine_slot_visible_to(slot, txid) {
            consider!(EventObjectRef::Primary(ObjectRef::Routine(slot)));
        }
    }
    for (slot, _) in storage.operators_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Operator(slot)));
    }
    for (slot, _) in storage.operator_families_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::OperatorFamily(slot)));
    }
    for (slot, _) in storage.operator_classes_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::OperatorClass(slot)));
    }
    for (slot, _) in storage.collations_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Collation(slot)));
    }
    for (slot, _) in storage.conversions_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Conversion(slot)));
    }
    for (slot, _) in storage.text_search_objects_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::TextSearch(slot)));
    }
    for slot in 0..storage.extended_statistics_count() {
        if storage.extended_statistics(slot).visible_to(txid) {
            consider!(EventObjectRef::Primary(ObjectRef::Statistics(slot)));
        }
    }
    for (slot, _) in storage.event_triggers_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::EventTrigger(slot)));
    }
    for (slot, _) in storage.triggers_with_slots_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Trigger(slot)));
    }
    for (slot, _) in storage.rules_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Rule(slot)));
    }
    for (slot, _) in storage.casts_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Cast(slot)));
    }
    for (slot, _) in storage.policies_with_slots_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Policy(slot)));
    }
    for (slot, _) in storage.publications_with_slots_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Publication(slot)));
    }
    for (slot, _) in
        storage.subscriptions_with_slots_visible_to(Some(storage.current_database_oid()), txid)
    {
        consider!(EventObjectRef::Primary(ObjectRef::Subscription(slot)));
    }
    for (slot, _) in storage.extensions_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::Extension(slot)));
    }
    for (slot, _) in storage.foreign_servers(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::ForeignServer(slot)));
    }
    for (slot, _) in storage.foreign_wrappers(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::ForeignDataWrapper(slot)));
    }
    for (slot, _) in storage.large_objects_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::LargeObject(slot)));
    }
    for slot in 0..storage.role_count() {
        if storage.role(slot).visible_to(txid) {
            consider!(EventObjectRef::Primary(ObjectRef::Role(slot)));
        }
    }
    for (_, method) in storage.access_methods_visible_to(txid) {
        consider!(EventObjectRef::Primary(ObjectRef::AccessMethod(
            method.oid().get()
        )));
    }
    if names.len() == 1 && arguments.is_empty() {
        if let Some(oid) = catalog::access_method_oid(names[0]) {
            consider!(EventObjectRef::Primary(ObjectRef::AccessMethod(oid)));
        }
        if let Some(oid) = catalog::procedural_language_oid(names[0]) {
            consider!(EventObjectRef::Primary(ObjectRef::ProceduralLanguage(oid)));
        }
    }

    if matches!(object_type, "function" | "procedure" | "aggregate") && names.len() == 2 {
        let mut identity = qualified(names[0], names[1])?;
        identity.write_char('(').map_err(|_| graph_full())?;
        for (index, argument) in arguments.iter().enumerate() {
            if index != 0 {
                identity.write_str(", ").map_err(|_| graph_full())?;
            }
            identity.write_str(argument).map_err(|_| graph_full())?;
        }
        identity.write_char(')').map_err(|_| graph_full())?;
        if let Some(oid) = catalog::routine_oid_by_name(storage, txid, identity.as_str(), true)?
            && let Some(object) = builtin_routine_object(oid)?
            && object.object_type.as_str() == object_type
        {
            return Ok((object.class_id, object.object_id, object.sub_id));
        }
    }

    if matches!(
        object_type,
        "table"
            | "table column"
            | "view"
            | "view column"
            | "materialized view"
            | "materialized view column"
            | "sequence"
            | "index"
            | "composite type"
    ) {
        let mut identity = EventIdentity::new();
        for (index, name) in names.iter().take(2).enumerate() {
            if index != 0 {
                identity.write_char('.').map_err(|_| graph_full())?;
            }
            identity.write_str(name).map_err(|_| graph_full())?;
        }
        Err(sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "relation \"{}\" does not exist",
            identity.as_str()
        ))
    } else {
        Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "object address for type \"{}\" does not exist",
            object_type
        ))
    }
}

fn relation_object_ref(storage: &Storage, txid: u32, object_id: i32) -> Option<EventObjectRef> {
    for slot in 0..storage.table_count() {
        if storage.table_slot_visible_to(slot, txid)
            && catalog::user_table_oid(storage, slot) == object_id
        {
            return Some(EventObjectRef::Primary(
                storage
                    .matview_slot_for_table(slot, txid)
                    .map_or(ObjectRef::Table(slot), ObjectRef::MaterializedView),
            ));
        }
        if storage.table_slot_visible_to(slot, txid)
            && storage.table_def(slot, txid).has_toast
            && catalog::toast_relation_oid(storage, slot) == object_id
        {
            return Some(EventObjectRef::ToastRelation(u16::try_from(slot).ok()?));
        }
    }
    for slot in 0..storage.view_count() {
        if storage.view_slot_visible_to(slot, txid) && catalog::view_oid(storage, slot) == object_id
        {
            return Some(EventObjectRef::Primary(ObjectRef::View(slot)));
        }
    }
    for slot in 0..storage.sequence_count() {
        if storage.sequence_slot_visible_to(slot, txid)
            && catalog::sequence_oid(storage, slot) == object_id
        {
            return Some(EventObjectRef::Primary(ObjectRef::Sequence(slot)));
        }
    }
    for (slot, _) in storage.composites_with_slots_visible_to(txid) {
        if catalog::named_composite_relation_oid(storage, slot) == object_id {
            return Some(EventObjectRef::CompositeRelation(slot as u16));
        }
    }
    if let Some((schema, name)) = catalog::index_identity_by_oid(storage, txid, object_id) {
        return Some(EventObjectRef::DetachedIndex {
            oid: object_id,
            schema,
            name,
        });
    }
    None
}

/// A TOAST relation's attributes, by attribute number.
const TOAST_COLUMNS: [&str; 3] = ["chunk_id", "chunk_seq", "chunk_data"];

fn relation_column_ref(
    storage: &Storage,
    txid: u32,
    relation: EventObjectRef,
    attnum: i32,
) -> Option<EventObjectRef> {
    let index = usize::try_from(attnum.checked_sub(1)?).ok()?;
    if let EventObjectRef::ToastRelation(table) = relation {
        return (index < TOAST_COLUMNS.len()).then_some(EventObjectRef::ToastColumn {
            table,
            attnum: u16::try_from(attnum).ok()?,
        });
    }
    let (reference, name) = match relation {
        EventObjectRef::Primary(reference @ ObjectRef::Table(slot)) => {
            let table = storage.table_def(slot, txid);
            let column = table.attribute_column(u16::try_from(attnum).ok()?)?;
            (reference, table.columns()[column].name)
        }
        EventObjectRef::Primary(reference @ ObjectRef::MaterializedView(slot)) => {
            let table = storage.table_def(storage.matview_table(slot), txid);
            let column = table.attribute_column(u16::try_from(attnum).ok()?)?;
            (reference, table.columns()[column].name)
        }
        EventObjectRef::Primary(reference @ ObjectRef::View(slot)) => (
            reference,
            *storage.view(slot).columns_for(txid).names().get(index)?,
        ),
        EventObjectRef::CompositeRelation(slot) => {
            let composite = storage.composite_for(usize::from(slot), txid);
            let field = composite
                .active_fields_for(txid)
                .find(|field| i32::from(field.attribute_number) == attnum)?;
            (ObjectRef::Composite(usize::from(slot)), field.name)
        }
        _ => return None,
    };
    Some(EventObjectRef::RelationColumn {
        relation: reference,
        attnum: u16::try_from(attnum).ok()?,
        name,
    })
}

fn type_object_ref(storage: &Storage, txid: u32, object_id: i32) -> Option<EventObjectRef> {
    // A named type is found by its creation stamp. Relation row types share
    // the composite OID band; they match no named composite and resolve below.
    if let Some((kind, creation, array)) = crate::storage::NamedTypeKind::from_oid(object_id) {
        use crate::storage::NamedTypeKind;
        let found = match kind {
            NamedTypeKind::Domain => (0..storage.domain_count()).find(|slot| {
                storage.domain_slot_visible_to(*slot, txid)
                    && storage.domain_for(*slot, txid).creation() == creation
            }),
            NamedTypeKind::Enum => (0..storage.enum_count()).find(|slot| {
                storage.enum_slot_visible_to(*slot, txid)
                    && storage.enum_for(*slot, txid).creation() == creation
            }),
            NamedTypeKind::Composite => storage
                .composites_with_slots_visible_to(txid)
                .find(|(_, composite)| composite.creation() == creation)
                .map(|(slot, _)| slot),
        };
        if let Some(slot) = found {
            let array_slot = u16::try_from(slot).ok()?;
            return Some(match (kind, array) {
                (NamedTypeKind::Domain, false) => EventObjectRef::Primary(ObjectRef::Domain(slot)),
                (NamedTypeKind::Enum, false) => EventObjectRef::Primary(ObjectRef::Enum(slot)),
                (NamedTypeKind::Composite, false) => {
                    EventObjectRef::Primary(ObjectRef::Composite(slot))
                }
                (NamedTypeKind::Domain, true) => EventObjectRef::DomainArray(array_slot),
                (NamedTypeKind::Enum, true) => EventObjectRef::EnumArray(array_slot),
                (NamedTypeKind::Composite, true) => EventObjectRef::CompositeArray(array_slot),
            });
        }
    }
    if let Some(slot) = catalog::table_slot_by_row_type_oid(storage, txid, object_id, false) {
        return Some(EventObjectRef::TableRowType(slot as u16));
    }
    if let Some(slot) = catalog::table_slot_by_row_type_oid(storage, txid, object_id, true) {
        return Some(EventObjectRef::TableArrayType(slot as u16));
    }
    if let Some(slot) = catalog::view_slot_by_row_type_oid(storage, txid, object_id, false) {
        return Some(EventObjectRef::ViewRowType(slot as u16));
    }
    if let Some(slot) = catalog::view_slot_by_row_type_oid(storage, txid, object_id, true) {
        return Some(EventObjectRef::ViewArrayType(slot as u16));
    }
    let column_type = crate::sql::types::ColType::from_oid(object_id)?;
    let name = match column_type {
        crate::sql::types::ColType::Array(element) => element.catalog_name(),
        scalar => scalar.catalog_name(),
    };
    Some(EventObjectRef::NamedType {
        oid: object_id,
        schema: crate::storage::SqlName::parse("pg_catalog").ok()?,
        name: crate::storage::SqlName::parse(name).ok()?,
    })
}

fn builtin_routine_object(object_id: i32) -> Result<Option<EventObject>, SqlError> {
    let Some(routine) = catalog::builtin_callable_by_oid(object_id) else {
        return Ok(None);
    };
    let name = routine.name;
    let object_type = match routine.kind {
        b'a' => "aggregate",
        b'p' => "procedure",
        _ => "function",
    };
    let mut identity = qualified(routine.schema, name)?;
    identity.write_char('(').map_err(|_| graph_full())?;
    let mut object = base_object(
        catalog::PG_PROC_OID,
        object_id,
        object_type,
        Some("pg_catalog"),
        None,
        StackStr::new(),
        false,
    );
    object.address_names[0] = StackStr::from_str(routine.schema);
    object.address_names[1] = StackStr::from_str(name);
    object.address_name_count = 2;
    for (index, oid) in routine.arguments.iter().copied().enumerate() {
        if index != 0 {
            identity.write_char(',').map_err(|_| graph_full())?;
        }
        let qualified_name = builtin_type_identity_name(oid)?;
        identity
            .write_str(qualified_name.as_str())
            .map_err(|_| graph_full())?;
        object.address_args[index] = qualified_name;
        object.address_arg_count += 1;
    }
    identity.write_char(')').map_err(|_| graph_full())?;
    object.identity = identity;
    Ok(Some(object))
}

/// Resolve a PostgreSQL catalog address through the same typed object graph
/// used by event triggers. DDL callbacks and SQL introspection therefore
/// cannot disagree after a rename or while observing transaction-local DDL.
/// PostgreSQL's `default acl` object: identity `for role r in schema s on
/// sequences`, address names `{r,s}` and the object-type letter as argument.
fn default_acl_object(storage: &Storage, txid: u32, object_id: i32) -> Option<EventObject> {
    use crate::storage::DefaultPrivilegeClass;
    let identity = storage.default_acl_identity_by_oid(object_id, txid)?;
    let owner = storage.role_name_by_oid(identity.owner, txid)?;
    let schema = match identity.schema {
        Some(schema) => Some(catalog::schema_name_by_oid(storage, txid, schema.oid())?),
        None => None,
    };
    let (objects, letter) = match identity.class {
        DefaultPrivilegeClass::Table => ("relations", "r"),
        DefaultPrivilegeClass::Sequence => ("sequences", "S"),
        DefaultPrivilegeClass::Function => ("functions", "f"),
        DefaultPrivilegeClass::Type => ("types", "T"),
        DefaultPrivilegeClass::Schema => ("schemas", "n"),
    };
    let mut text = EventIdentity::new();
    write!(text, "for role {}", identifier(owner.as_str()).as_str()).ok()?;
    if let Some(schema) = schema {
        write!(text, " in schema {}", identifier(schema.as_str()).as_str()).ok()?;
    }
    write!(text, " on {objects}").ok()?;
    if text.is_truncated() {
        return None;
    }
    let mut object = EventObject {
        class_id: 826,
        object_id,
        object_type: StackStr::from_str("default acl"),
        identity: text,
        ..EventObject::EMPTY
    };
    object.address_names[0] = StackStr::from_str(owner.as_str());
    object.address_name_count = 1;
    if let Some(schema) = schema {
        object.address_names[1] = StackStr::from_str(schema.as_str());
        object.address_name_count = 2;
    }
    object.address_args[0] = StackStr::from_str(letter);
    object.address_arg_count = 1;
    Some(object)
}

fn existing_catalog_object(
    storage: &Storage,
    txid: u32,
    class_id: i32,
    object_id: i32,
    sub_id: i32,
) -> Result<EventObject, SqlError> {
    if class_id == 826 {
        return default_acl_object(storage, txid, object_id)
            .filter(|_| sub_id == 0)
            .ok_or_else(|| catalog_lookup_failed(class_id, object_id));
    }
    let reference = match class_id {
        catalog::PG_CLASS_OID => {
            let relation = relation_object_ref(storage, txid, object_id)
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?;
            if sub_id == 0 {
                relation
            } else {
                return relation_column_ref(storage, txid, relation, sub_id)
                    .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?
                    .materialize(storage, txid);
            }
        }
        catalog::PG_TYPE_OID => type_object_ref(storage, txid, object_id)
            .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        catalog::PG_PROC_OID => {
            if let Some(slot) = storage.routine_slot_by_oid(object_id, txid) {
                EventObjectRef::Primary(ObjectRef::Routine(slot))
            } else {
                return builtin_routine_object(object_id)?
                    .ok_or_else(|| catalog_lookup_failed(class_id, object_id));
            }
        }
        catalog::PG_NAMESPACE_OID => {
            if let Some(slot) = (0..storage.schema_count()).find(|slot| {
                storage.schema_def(*slot).visible_to(txid)
                    && catalog::namespace_oid_for_slot(storage, *slot) == object_id
            }) {
                EventObjectRef::Primary(ObjectRef::Schema(slot))
            } else {
                let name = match object_id {
                    11 => "pg_catalog",
                    99 => "pg_toast",
                    _ => return Err(catalog_lookup_failed(class_id, object_id)),
                };
                return Ok(base_object(
                    catalog::PG_NAMESPACE_OID,
                    object_id,
                    "schema",
                    None,
                    Some(name),
                    StackStr::from_str(name),
                    false,
                ));
            }
        }
        catalog::PG_CONSTRAINT_OID => {
            if let Some((table, name)) =
                catalog::table_constraint_identity_by_oid(storage, txid, object_id)
            {
                EventObjectRef::TableConstraint {
                    table: table as u16,
                    oid: object_id,
                    name,
                }
            } else if let Some((domain, constraint)) =
                (0..storage.domain_count()).find_map(|domain| {
                    if !storage.domain_slot_visible_to(domain, txid) {
                        return None;
                    }
                    storage
                        .domain_for(domain, txid)
                        .constraint_identities()
                        .find_map(|identity| {
                            identity
                                .filter(|identity| identity.oid() == object_id)
                                .map(|identity| (domain, identity))
                        })
                })
            {
                EventObjectRef::DomainConstraint {
                    domain: domain as u16,
                    constraint,
                }
            } else {
                return Err(catalog_lookup_failed(class_id, object_id));
            }
        }
        catalog::PG_OPERATOR_OID => EventObjectRef::Primary(ObjectRef::Operator(
            storage
                .operator_slot_by_oid(object_id, txid)
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_OPFAMILY_OID => {
            if let Some(slot) = storage.operator_family_slot_by_oid(object_id, txid) {
                EventObjectRef::Primary(ObjectRef::OperatorFamily(slot))
            } else {
                let (method, name) = catalog::builtin_operator_family(object_id)
                    .filter(|_| sub_id == 0)
                    .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?;
                return builtin_access_method_object(class_id, object_id, method, name);
            }
        }
        catalog::PG_OPCLASS_OID => {
            if let Some(slot) = crate::storage::OperatorClassOid::parse(object_id)
                .and_then(|oid| storage.operator_class_slot_by_oid(oid, txid))
            {
                EventObjectRef::Primary(ObjectRef::OperatorClass(slot))
            } else {
                let (method, name) = catalog::builtin_operator_class(object_id)
                    .filter(|_| sub_id == 0)
                    .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?;
                return builtin_access_method_object(class_id, object_id, method, name);
            }
        }
        catalog::PG_COLLATION_OID => EventObjectRef::Primary(ObjectRef::Collation(
            storage
                .collations_visible_to(txid)
                .find_map(|(slot, _)| (storage.collation(slot).oid() == object_id).then_some(slot))
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_CONVERSION_OID => EventObjectRef::Primary(ObjectRef::Conversion(
            storage
                .conversions_visible_to(txid)
                .find_map(|(slot, _)| (storage.conversion(slot).oid() == object_id).then_some(slot))
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_STATISTIC_EXT_OID => EventObjectRef::Primary(ObjectRef::Statistics(
            storage
                .extended_statistics_visible(txid)
                .find_map(|(slot, _)| {
                    (storage.extended_statistics(slot).oid() == object_id).then_some(slot)
                })
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_TS_PARSER_OID
        | catalog::PG_TS_TEMPLATE_OID
        | catalog::PG_TS_DICT_OID
        | catalog::PG_TS_CONFIG_OID => EventObjectRef::Primary(ObjectRef::TextSearch(
            storage
                .text_search_objects_visible_to(txid)
                .find_map(|(slot, object)| {
                    (object.oid() == object_id
                        && match object.kind() {
                            crate::sql::ast::TextSearchObjectKind::Parser => {
                                class_id == catalog::PG_TS_PARSER_OID
                            }
                            crate::sql::ast::TextSearchObjectKind::Template => {
                                class_id == catalog::PG_TS_TEMPLATE_OID
                            }
                            crate::sql::ast::TextSearchObjectKind::Dictionary => {
                                class_id == catalog::PG_TS_DICT_OID
                            }
                            crate::sql::ast::TextSearchObjectKind::Configuration => {
                                class_id == catalog::PG_TS_CONFIG_OID
                            }
                        })
                    .then_some(slot)
                })
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        3466 => EventObjectRef::Primary(ObjectRef::EventTrigger(
            storage
                .event_triggers_visible_to(txid)
                .find_map(|(slot, _)| {
                    (storage.event_trigger(slot).oid() == object_id).then_some(slot)
                })
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_TRIGGER_OID => EventObjectRef::Primary(ObjectRef::Trigger(
            storage
                .triggers_with_slots_visible_to(txid)
                .find_map(|(slot, trigger)| {
                    (crate::storage::trigger_oid(&trigger) == object_id).then_some(slot)
                })
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_REWRITE_OID => {
            if let Some(slot) = storage.matview_slot_by_rule_oid(object_id, txid) {
                EventObjectRef::MatviewRule(u16::try_from(slot).map_err(|_| graph_full())?)
            } else {
                EventObjectRef::Primary(ObjectRef::Rule(
                    storage
                        .rules_visible_to(txid)
                        .find_map(|(slot, rule)| (rule.oid() == object_id).then_some(slot))
                        .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
                ))
            }
        }
        catalog::PG_CAST_OID => {
            if let Some(slot) = storage
                .casts_visible_to(txid)
                .find_map(|(slot, cast)| (cast.oid() == object_id).then_some(slot))
            {
                EventObjectRef::Primary(ObjectRef::Cast(slot))
            } else {
                let (source, target) = catalog::cast_type_oids(storage, txid, object_id)
                    .filter(|_| sub_id == 0)
                    .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?;
                return builtin_cast_object(object_id, source, target);
            }
        }
        catalog::PG_POLICY_OID => EventObjectRef::Primary(ObjectRef::Policy(
            storage
                .policies_with_slots_visible_to(txid)
                .find_map(|(slot, policy)| {
                    (crate::storage::policy_oid(&policy) == object_id).then_some(slot)
                })
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_PUBLICATION_OID => EventObjectRef::Primary(ObjectRef::Publication(
            storage
                .publications_with_slots_visible_to(txid)
                .find_map(|(slot, _)| {
                    (storage.publication_at(slot).oid() == object_id).then_some(slot)
                })
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_SUBSCRIPTION_OID => EventObjectRef::Primary(ObjectRef::Subscription(
            storage
                .subscriptions_with_slots_visible_to(None, txid)
                .find_map(|(slot, subscription)| (subscription.oid() == object_id).then_some(slot))
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_EXTENSION_OID => EventObjectRef::Primary(ObjectRef::Extension(
            storage
                .extensions_visible_to(txid)
                .find_map(|(slot, extension)| (extension.oid() == object_id).then_some(slot))
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_FOREIGN_SERVER_OID => EventObjectRef::Primary(ObjectRef::ForeignServer(
            storage
                .foreign_servers(txid)
                .find_map(|(slot, entry)| (entry.oid() == object_id).then_some(slot))
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_FOREIGN_DATA_WRAPPER_OID => {
            EventObjectRef::Primary(ObjectRef::ForeignDataWrapper(
                storage
                    .foreign_wrappers(txid)
                    .find_map(|(slot, entry)| (entry.oid() == object_id).then_some(slot))
                    .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
            ))
        }
        catalog::PG_LARGEOBJECT_OID => EventObjectRef::Primary(ObjectRef::LargeObject(
            storage
                .large_objects_visible_to(txid)
                .find_map(|(slot, object)| (object.oid.get() as i32 == object_id).then_some(slot))
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        1260 => EventObjectRef::Primary(ObjectRef::Role(
            storage
                .role_slot_by_oid(object_id, txid)
                .ok_or_else(|| catalog_lookup_failed(class_id, object_id))?,
        )),
        catalog::PG_AM_OID => {
            if catalog::access_method_name(object_id).is_none()
                && storage.access_method_name_by_oid(object_id).is_none()
            {
                return Err(catalog_lookup_failed(class_id, object_id));
            }
            EventObjectRef::Primary(ObjectRef::AccessMethod(object_id))
        }
        catalog::PG_LANGUAGE_OID => {
            if catalog::procedural_language_name(object_id).is_none() {
                return Err(catalog_lookup_failed(class_id, object_id));
            }
            EventObjectRef::Primary(ObjectRef::ProceduralLanguage(object_id))
        }
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "unsupported object class: {}",
                class_id
            ));
        }
    };
    if sub_id != 0 {
        return Err(catalog_lookup_failed(class_id, object_id));
    }
    reference.materialize(storage, txid)
}

pub(crate) fn catalog_object(
    storage: &Storage,
    txid: u32,
    class_id: i32,
    object_id: i32,
    sub_id: i32,
) -> Result<EventObject, SqlError> {
    match existing_catalog_object(storage, txid, class_id, object_id, sub_id) {
        Ok(object) => Ok(object),
        Err(error) if error.sqlstate == sqlstate::UNDEFINED_OBJECT => {
            // A missing attribute of an existing relation is still typed by
            // its relation's kind (`table column`), as in PostgreSQL.
            if class_id == catalog::PG_CLASS_OID
                && sub_id != 0
                && let Some(relation) = relation_object_ref(storage, txid, object_id)
            {
                let relation = relation.materialize(storage, txid)?;
                let mut object_type = StackStr::new();
                write!(object_type, "{} column", relation.object_type.as_str())
                    .map_err(|_| graph_full())?;
                return Ok(EventObject {
                    class_id,
                    object_id,
                    sub_id,
                    object_type,
                    ..EventObject::EMPTY
                });
            }
            let object_type = match class_id {
                catalog::PG_CLASS_OID => "relation",
                catalog::PG_PROC_OID => "routine",
                catalog::PG_TYPE_OID => "type",
                catalog::PG_NAMESPACE_OID => "schema",
                catalog::PG_CONSTRAINT_OID => "constraint",
                catalog::PG_OPERATOR_OID => "operator",
                catalog::PG_OPFAMILY_OID => "operator family",
                catalog::PG_OPCLASS_OID => "operator class",
                catalog::PG_COLLATION_OID => "collation",
                catalog::PG_CONVERSION_OID => "conversion",
                catalog::PG_STATISTIC_EXT_OID => "statistics object",
                catalog::PG_TS_PARSER_OID => "text search parser",
                catalog::PG_TS_TEMPLATE_OID => "text search template",
                catalog::PG_TS_DICT_OID => "text search dictionary",
                catalog::PG_TS_CONFIG_OID => "text search configuration",
                3466 => "event trigger",
                catalog::PG_TRIGGER_OID => "trigger",
                catalog::PG_REWRITE_OID => "rule",
                catalog::PG_CAST_OID => "cast",
                catalog::PG_POLICY_OID => "policy",
                catalog::PG_PUBLICATION_OID => "publication",
                catalog::PG_SUBSCRIPTION_OID => "subscription",
                catalog::PG_FOREIGN_SERVER_OID => "server",
                catalog::PG_FOREIGN_DATA_WRAPPER_OID => "foreign-data wrapper",
                catalog::PG_EXTENSION_OID => "extension",
                catalog::PG_LARGEOBJECT_OID => "large object",
                1260 => "role",
                catalog::PG_AM_OID => "access method",
                catalog::PG_LANGUAGE_OID => "language",
                _ => return Err(error),
            };
            Ok(EventObject {
                class_id,
                object_id,
                sub_id,
                object_type: StackStr::from_str(object_type),
                ..EventObject::EMPTY
            })
        }
        Err(error) => Err(error),
    }
}

fn capture_original_drops<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    arena: &'a Arena,
) -> Result<&'a [ObjectRef], SqlError> {
    use crate::sql::ast::{RoutineIdentity, RoutineTargetKind};
    let mut roots = ArenaList::new(arena);
    let mut remember = |reference: Option<ObjectRef>| -> Result<(), SqlError> {
        if let Some(reference) = reference
            && !roots.as_slice().contains(&reference)
        {
            roots.push(reference).map_err(|_| graph_full())?;
        }
        Ok(())
    };
    let relation = |name: crate::sql::ast::QualName<'_>| {
        let (schema, _) = storage.classify_relation(name.schema, name.name, txid)?;
        relation_reference(storage, txid, schema.as_str(), name.name)
    };
    let routine = |identity: &RoutineIdentity<'_>| {
        crate::sql::exec::lookup_routine(storage, txid, RoutineTargetKind::Either, identity).map(
            |found| match found {
                Ok(crate::sql::exec::RoutineTarget::Stored(slot)) => Some(ObjectRef::Routine(slot)),
                Ok(crate::sql::exec::RoutineTarget::Builtin(_)) | Err(_) => None,
            },
        )
    };
    match statement {
        Stmt::DropTable(drop) | Stmt::DropForeignTable(drop) => {
            for name in drop.names {
                remember(relation(*name))?;
            }
        }
        Stmt::DropView { names, .. } | Stmt::DropMaterializedView { names, .. } => {
            for name in *names {
                remember(relation(*name))?;
            }
        }
        Stmt::DropSequence { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .sequence_on_path(name.schema, name.name, txid)
                        .map(ObjectRef::Sequence),
                )?;
            }
        }
        Stmt::DropDomain { names, .. } | Stmt::DropType { names, .. } => {
            for name in *names {
                let domain = match name.schema {
                    Some(schema) => storage.domain_slot(schema, name.name, txid),
                    None => storage.resolve_domain_slot(name.name, txid),
                };
                if let Some(slot) = domain {
                    remember(Some(ObjectRef::Domain(slot)))?;
                    continue;
                }
                if matches!(statement, Stmt::DropDomain { .. }) {
                    continue;
                }
                let enumeration = match name.schema {
                    Some(schema) => storage.enum_slot(schema, name.name, txid),
                    None => storage.resolve_enum_slot(name.name, txid),
                };
                if let Some(slot) = enumeration {
                    remember(Some(ObjectRef::Enum(slot)))?;
                    continue;
                }
                let composite = match name.schema {
                    Some(schema) => storage.composite_slot(schema, name.name, txid),
                    None => storage.resolve_composite_slot(name.name, txid),
                };
                remember(composite.map(ObjectRef::Composite))?;
            }
        }
        Stmt::DropIndex { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .resolve_index_slot(name.schema, name.name, txid)
                        .map(ObjectRef::Index),
                )?;
            }
        }
        Stmt::DropSchema { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .find_schema_visible(name, txid)
                        .map(ObjectRef::Schema),
                )?;
            }
        }
        Stmt::DropFunction { functions, .. }
        | Stmt::DropProcedure {
            procedures: functions,
            ..
        }
        | Stmt::DropRoutine {
            routines: functions,
            ..
        } => {
            for identity in *functions {
                remember(routine(identity)?)?;
            }
        }
        Stmt::DropAggregate { aggregates, .. } => {
            for identity in *aggregates {
                remember(routine(&identity.routine())?)?;
            }
        }
        Stmt::DropCast(cast) => {
            let source = crate::sql::exec::resolve_routine_type(storage, txid, cast.source_type)?;
            let target = crate::sql::exec::resolve_routine_type(storage, txid, cast.target_type)?;
            remember(storage.cast_slot(source, target, txid).map(ObjectRef::Cast))?;
        }
        Stmt::DropOperator { identities, .. } => {
            for identity in *identities {
                match crate::sql::exec::resolve_operator_identity(storage, txid, *identity) {
                    Ok((slot, _)) => remember(Some(ObjectRef::Operator(slot)))?,
                    Err(error) if error.sqlstate == sqlstate::UNDEFINED_FUNCTION => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Stmt::DropOperatorFamily { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .operator_family_slot_on_path(name.schema, name.name, txid)
                        .map(ObjectRef::OperatorFamily),
                )?;
            }
        }
        Stmt::DropOperatorClass { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .operator_class_slot_on_path(name.schema, name.name, txid)
                        .map(ObjectRef::OperatorClass),
                )?;
            }
        }
        Stmt::DropStatistics { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .extended_statistics_slot_on_path(name.schema, name.name, txid)
                        .map(ObjectRef::Statistics),
                )?;
            }
        }
        Stmt::DropCollation { name, .. } => remember(
            storage
                .collation_slot_on_path(name.schema, name.name, txid)
                .map(ObjectRef::Collation),
        )?,
        Stmt::DropConversion { name, .. } => remember(
            storage
                .conversion_slot_on_path(name.schema, name.name, txid)
                .map(ObjectRef::Conversion),
        )?,
        Stmt::DropTextSearch { kind, name, .. } => remember(
            storage
                .text_search_slot_on_path(*kind, name.schema, name.name, txid)
                .map(ObjectRef::TextSearch),
        )?,
        Stmt::DropPolicy { policy, .. } => {
            if let Some(ObjectRef::Table(table)) = relation(policy.table) {
                remember(
                    storage
                        .policy_slot_on(table, policy.name, txid)
                        .map(ObjectRef::Policy),
                )?;
            }
        }
        Stmt::DropTrigger { trigger, .. } => {
            let target = match relation(trigger.table) {
                Some(ObjectRef::Table(table)) => {
                    Some(crate::storage::TriggerTarget::Table(table as u16))
                }
                Some(ObjectRef::View(view)) => {
                    Some(crate::storage::TriggerTarget::View(view as u16))
                }
                _ => None,
            };
            if let Some(target) = target {
                remember(
                    storage
                        .trigger_slot_on(target, trigger.name, txid)
                        .map(ObjectRef::Trigger),
                )?;
            }
        }
        Stmt::DropRule(rule) => {
            let target = match relation(rule.table) {
                Some(ObjectRef::Table(table)) => {
                    Some(crate::storage::RuleTarget::Table(table as u16))
                }
                Some(ObjectRef::View(view)) => Some(crate::storage::RuleTarget::View(view as u16)),
                _ => None,
            };
            if let Some(target) = target {
                remember(
                    storage
                        .rule_slot(target, rule.name, txid)
                        .map(ObjectRef::Rule),
                )?;
            }
        }
        Stmt::DropPublication { names, .. } => {
            for (slot, definition) in storage.publications_with_slots_visible_to(txid) {
                if names.contains(&definition.name_for(txid).as_str()) {
                    remember(Some(ObjectRef::Publication(slot)))?;
                }
            }
        }
        Stmt::DropSubscription { names, .. } => {
            for (slot, definition) in storage
                .subscriptions_with_slots_visible_to(Some(storage.current_database_oid()), txid)
            {
                if names.contains(&definition.name_for(txid).as_str()) {
                    remember(Some(ObjectRef::Subscription(slot)))?;
                }
            }
        }
        Stmt::DropExtension { names, .. } => {
            for name in *names {
                remember(storage.extension_slot(name, txid).map(ObjectRef::Extension))?;
            }
        }
        Stmt::DropAccessMethod { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .access_method_slot(name, txid)
                        .and_then(|slot| storage.access_method_oid_at(slot))
                        .map(ObjectRef::AccessMethod),
                )?;
            }
        }
        Stmt::DropEventTrigger { name, .. } => remember(
            storage
                .event_trigger_slot(name, txid)
                .map(ObjectRef::EventTrigger),
        )?,
        Stmt::DropForeignDataWrapper { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .foreign_wrapper(name, txid)
                        .map(|(slot, _)| ObjectRef::ForeignDataWrapper(slot)),
                )?;
            }
        }
        Stmt::DropForeignServer { names, .. } => {
            for name in *names {
                remember(
                    storage
                        .foreign_server(name, txid)
                        .map(|(slot, _)| ObjectRef::ForeignServer(slot)),
                )?;
            }
        }
        _ => {}
    }
    Ok(roots.as_slice())
}

fn push_command(
    output: &mut ArenaList<'_, DdlCommand>,
    count: &mut usize,
    reference: EventObjectRef,
    tag: &str,
    in_extension: bool,
) -> Result<(), SqlError> {
    output
        .push(DdlCommand {
            reference: CommandReference::Object(reference),
            command_tag: StackStr::from_str(tag),
            in_extension,
        })
        .map_err(|_| graph_full())?;
    *count += 1;
    Ok(())
}

fn push_drop(
    output: &mut ArenaList<'_, DroppedObject>,
    count: &mut usize,
    reference: EventObjectRef,
    original: bool,
    normal: bool,
) -> Result<(), SqlError> {
    output
        .push(DroppedObject {
            reference,
            original,
            normal,
            temporary: false,
        })
        .map_err(|_| graph_full())?;
    *count += 1;
    Ok(())
}

fn push_drop_once(
    output: &mut ArenaList<'_, DroppedObject>,
    count: &mut usize,
    reference: EventObjectRef,
    original: bool,
    normal: bool,
) -> Result<(), SqlError> {
    if output
        .as_slice()
        .iter()
        .any(|dropped| dropped.reference == reference)
    {
        return Ok(());
    }
    push_drop(output, count, reference, original, normal)
}

fn detached_constraint<Definition0: crate::storage::DefinitionBacking>(
    table: &crate::storage::TableDefinition<Definition0>,
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

fn detached_index<Definition0: crate::storage::DefinitionBacking>(
    table: &crate::storage::TableDefinition<Definition0>,
    oid: i32,
    name: impl AsRef<str>,
) -> EventObjectRef {
    EventObjectRef::DetachedIndex {
        oid,
        schema: table.schema,
        name: StackStr::from_str(name.as_ref()),
    }
}

fn foreign_key_trigger_reference<Definition0: crate::storage::DefinitionBacking>(
    child: &crate::storage::TableDefinition<Definition0>,
    foreign_key: &crate::storage::ForeignKey,
    trigger: crate::storage::ForeignKeyTrigger,
) -> EventObjectRef {
    let oid = foreign_key.trigger_oid(trigger);
    let ordinal = trigger.ordinal();
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

fn push_foreign_key_trigger_drops<Definition0: crate::storage::DefinitionBacking>(
    child: &crate::storage::TableDefinition<Definition0>,
    foreign_key: &crate::storage::ForeignKey,
    output: &mut ArenaList<'_, DroppedObject>,
    count: &mut usize,
) -> Result<(), SqlError> {
    for trigger in crate::storage::ForeignKeyTrigger::ALL {
        push_drop_once(
            output,
            count,
            foreign_key_trigger_reference(child, foreign_key, trigger),
            false,
            false,
        )?;
    }
    Ok(())
}

fn table_index_positions<Definition0: crate::storage::DefinitionBacking>(
    table: &crate::storage::TableDefinition<Definition0>,
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
    before: &BeforeDdl<'_>,
    storage: &Storage,
    txid: u32,
    output: &mut ArenaList<'_, DroppedObject>,
    count: &mut usize,
) -> Result<(), SqlError> {
    let (slot, definitions) = match before.altered_table.as_ref() {
        Some(state) => state,
        None => return Ok(()),
    };
    let slot = *slot;
    let old = definitions.definition(storage, slot, txid)?;
    let index_reference = |oid: i32, name: &str| {
        before
            .indexes
            .iter()
            .find_map(|&(index_slot, identity)| {
                (identity.oid() == oid
                    && storage
                        .dropped_table_index_for_event(index_slot, slot, txid)
                        .is_some_and(|index| index.created_at == identity.created_at()))
                .then_some(EventObjectRef::Primary(ObjectRef::Index(index_slot)))
            })
            .unwrap_or_else(|| detached_index(old, oid, name))
    };
    let Stmt::AlterTable(alter) = statement else {
        return Ok(());
    };
    let new = storage.table_def(slot, txid);
    let table_oid = catalog::user_table_oid(storage, slot);
    let (inline_positions, named_start) = table_index_positions(old);

    let constraint_removed = |name: &str| {
        !new.checks().iter().any(|item| item.name.as_str() == name)
            && !new.fkeys().iter().any(|item| item.name.as_str() == name)
            && !new.uniques().iter().any(|item| item.name.as_str() == name)
            && !new
                .exclusions()
                .iter()
                .any(|item| item.name.as_str() == name)
            && !new.columns().iter().enumerate().any(|(index, column)| {
                ((column.primary || column.unique)
                    && new
                        .column_key_name(index)
                        .expect("admitted inline key has a name")
                        .as_str()
                        == name)
                    || (column.not_null.is_required()
                        && not_null_name(&new, column).as_str() == name)
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
                        attnum: old.attribute_number(column_index),
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
                            old.not_null_constraint_oid(column_index),
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
                        detached_constraint(old, old.checks()[index].oid(), *name),
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
                        detached_constraint(old, old.fkeys()[index].oid(), *name),
                        true,
                        false,
                    )?;
                } else if let Some(index) = old
                    .uniques()
                    .iter()
                    .position(|constraint| constraint.name.as_str() == *name)
                {
                    let index_oid = old.implicit_index_oid(named_start + index);
                    push_drop_once(
                        output,
                        count,
                        detached_constraint(
                            old,
                            old.uniques()[index]
                                .identity
                                .expect("admitted unique key has a constraint identity")
                                .oid(),
                            *name,
                        ),
                        true,
                        false,
                    )?;
                    push_drop_once(
                        output,
                        count,
                        index_reference(index_oid, name),
                        false,
                        false,
                    )?;
                } else if let Some(index) = old
                    .exclusions()
                    .iter()
                    .position(|constraint| constraint.name.as_str() == *name)
                {
                    let index_oid =
                        old.implicit_index_oid(named_start + old.uniques().len() + index);
                    push_drop_once(
                        output,
                        count,
                        detached_constraint(
                            old,
                            old.exclusions()[index]
                                .identity
                                .expect("admitted exclusion has a constraint identity")
                                .oid(),
                            *name,
                        ),
                        true,
                        false,
                    )?;
                    push_drop_once(
                        output,
                        count,
                        index_reference(index_oid, name),
                        false,
                        false,
                    )?;
                } else {
                    for (column_index, column) in old.columns().iter().enumerate() {
                        let generated = if column.primary || column.unique {
                            Some(StackStr::from_str(
                                old.column_key_name(column_index)
                                    .expect("admitted inline key has a name")
                                    .as_str(),
                            ))
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
                                let index_oid = old.implicit_index_oid(usize::from(
                                    inline_positions[column_index].ok_or_else(graph_full)?,
                                ));
                                push_drop_once(
                                    output,
                                    count,
                                    detached_constraint(
                                        old,
                                        old.column_constraint_identity(column_index)
                                            .expect("admitted inline key has a constraint identity")
                                            .oid(),
                                        *name,
                                    ),
                                    true,
                                    false,
                                )?;
                                push_drop_once(
                                    output,
                                    count,
                                    index_reference(index_oid, name),
                                    false,
                                    false,
                                )?;
                            } else {
                                push_drop_once(
                                    output,
                                    count,
                                    detached_constraint(
                                        old,
                                        old.not_null_constraint_oid(column_index),
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
                detached_constraint(old, old.checks()[index].oid(), check.name.as_str()),
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
                detached_constraint(old, old.fkeys()[index].oid(), foreign_key.name.as_str()),
                false,
                true,
            )?;
            push_foreign_key_trigger_drops(old, foreign_key, output, count)?;
        }
    }
    for (column_index, column) in old.columns().iter().enumerate() {
        let survives = new
            .attribute_column(old.attribute_number(column_index))
            .map(|index| &new.columns()[index]);
        if column.not_null.is_required()
            && survives.is_none_or(|new_column| !new_column.not_null.is_required())
        {
            let name = not_null_name(old, column);
            push_drop_once(
                output,
                count,
                detached_constraint(
                    old,
                    old.not_null_constraint_oid(column_index),
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
            let name = old
                .column_key_name(column_index)
                .expect("admitted inline key has a name");
            let index_oid = old.implicit_index_oid(usize::from(
                inline_positions[column_index].ok_or_else(graph_full)?,
            ));
            push_drop_once(
                output,
                count,
                detached_constraint(
                    old,
                    old.column_constraint_identity(column_index)
                        .expect("admitted inline key has a constraint identity")
                        .oid(),
                    name.as_str(),
                ),
                false,
                false,
            )?;
            push_drop_once(
                output,
                count,
                index_reference(index_oid, name.as_str()),
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
            let index_oid = old.implicit_index_oid(named_start + index);
            push_drop_once(
                output,
                count,
                detached_constraint(
                    old,
                    unique
                        .identity
                        .expect("admitted unique key has a constraint identity")
                        .oid(),
                    unique.name.as_str(),
                ),
                false,
                false,
            )?;
            push_drop_once(
                output,
                count,
                index_reference(index_oid, unique.name.as_str()),
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
            let index_oid = old.implicit_index_oid(named_start + old.uniques().len() + index);
            push_drop_once(
                output,
                count,
                detached_constraint(
                    old,
                    exclusion
                        .identity
                        .expect("admitted exclusion has a constraint identity")
                        .oid(),
                    exclusion.name.as_str(),
                ),
                false,
                false,
            )?;
            push_drop_once(
                output,
                count,
                index_reference(index_oid, exclusion.name.as_str()),
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
    output: &mut ArenaList<'_, DdlCommand>,
    count: &mut usize,
    in_extension: bool,
) -> Result<(), SqlError> {
    let table = storage.table_def(slot, txid);
    let mut position = 0usize;
    for (column_index, column) in table.columns().iter().enumerate() {
        let name = (column.primary || column.unique).then(|| {
            StackStr::from_str(
                table
                    .column_key_name(column_index)
                    .expect("admitted inline key has a name")
                    .as_str(),
            )
        });
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

/// A table's dependents, or a materialized view's (`matview` names it, and
/// its `_RETURN` rule follows its row types; `parent_is_normal` is the
/// rule's `normal` flag).
fn push_table_drop_dependents(
    storage: &Storage,
    txid: u32,
    slot: usize,
    matview: Option<(usize, bool)>,
    indexes: &[(usize, crate::storage::RelationIdentity)],
    output: &mut ArenaList<'_, DroppedObject>,
    count: &mut usize,
) -> Result<(), SqlError> {
    let table = storage.table_def(slot, txid);
    let table_ref = u16::try_from(slot).map_err(|_| graph_full())?;
    let index_reference =
        |position: usize, name: StackStr<128>| -> Result<EventObjectRef, SqlError> {
            let oid = table.implicit_index_oid(position);
            for &(index_slot, identity) in indexes {
                if let Some(index) = storage.dropped_table_index_for_event(index_slot, slot, txid)
                    && index.created_at == identity.created_at()
                    && catalog::explicit_index_oid(&index) == oid
                {
                    return Ok(EventObjectRef::Primary(ObjectRef::Index(index_slot)));
                }
            }
            Ok(EventObjectRef::TableIndex {
                table: table_ref,
                position: u16::try_from(position).map_err(|_| graph_full())?,
                name,
            })
        };
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
    if let Some((matview, parent_is_normal)) = matview {
        push_drop(
            output,
            count,
            EventObjectRef::MatviewRule(u16::try_from(matview).map_err(|_| graph_full())?),
            false,
            parent_is_normal,
        )?;
    }

    for check in table.checks() {
        push_drop(
            output,
            count,
            EventObjectRef::TableConstraint {
                table: table_ref,
                oid: check.oid(),
                name: StackStr::from_str(check.name.as_str()),
            },
            false,
            true,
        )?;
    }
    for (index, column) in table.columns().iter().enumerate() {
        if column.not_null.is_required() {
            let name = not_null_name(&table, column);
            push_drop(
                output,
                count,
                EventObjectRef::TableConstraint {
                    table: table_ref,
                    oid: table.not_null_constraint_oid(index),
                    name,
                },
                false,
                false,
            )?;
        }
    }
    for foreign_key in table.fkeys() {
        push_drop(
            output,
            count,
            EventObjectRef::TableConstraint {
                table: table_ref,
                oid: foreign_key.oid(),
                name: StackStr::from_str(foreign_key.name.as_str()),
            },
            false,
            true,
        )?;
        push_foreign_key_trigger_drops(&table, foreign_key, output, count)?;
    }
    if table.has_toast {
        push_drop(
            output,
            count,
            EventObjectRef::ToastRelation(table_ref),
            false,
            false,
        )?;
        let anchor = table
            .toast_index_identity
            .expect("admitted TOAST index has an identity");
        let registered_anchor = indexes.iter().any(|&(index_slot, identity)| {
            identity == anchor
                && storage
                    .dropped_table_index_for_event(index_slot, slot, txid)
                    .is_some_and(|index| {
                        index.relation_kind() == crate::storage::IndexRelationKind::Toast
                    })
        });
        if !registered_anchor {
            push_drop(
                output,
                count,
                EventObjectRef::ToastIndex(table_ref),
                false,
                false,
            )?;
        }
    }

    let mut position = 0usize;
    for (column_index, column) in table.columns().iter().enumerate() {
        let name = (column.primary || column.unique).then(|| {
            StackStr::from_str(
                table
                    .column_key_name(column_index)
                    .expect("admitted inline key has a name")
                    .as_str(),
            )
        });
        if let Some(name) = name {
            push_drop(
                output,
                count,
                EventObjectRef::TableConstraint {
                    table: table_ref,
                    oid: table
                        .column_constraint_identity(column_index)
                        .expect("admitted inline key has a constraint identity")
                        .oid(),
                    name,
                },
                false,
                false,
            )?;
            push_drop(
                output,
                count,
                index_reference(position, name)?,
                false,
                false,
            )?;
            position += 1;
        }
    }
    for unique in table.uniques() {
        push_drop(
            output,
            count,
            EventObjectRef::TableConstraint {
                table: table_ref,
                oid: unique
                    .identity
                    .expect("admitted unique key has a constraint identity")
                    .oid(),
                name: StackStr::from_str(unique.name.as_str()),
            },
            false,
            false,
        )?;
        push_drop(
            output,
            count,
            index_reference(position, StackStr::from_str(unique.name.as_str()))?,
            false,
            false,
        )?;
        position += 1;
    }
    for exclusion in table.exclusions() {
        push_drop(
            output,
            count,
            EventObjectRef::TableConstraint {
                table: table_ref,
                oid: exclusion
                    .identity
                    .expect("admitted exclusion has a constraint identity")
                    .oid(),
                name: StackStr::from_str(exclusion.name.as_str()),
            },
            false,
            false,
        )?;
        push_drop(
            output,
            count,
            index_reference(position, StackStr::from_str(exclusion.name.as_str()))?,
            false,
            false,
        )?;
        position += 1;
    }
    for &(index_slot, identity) in indexes {
        if let Some(index) = storage.dropped_table_index_for_event(index_slot, slot, txid)
            && index.created_at == identity.created_at()
        {
            push_drop_once(
                output,
                count,
                EventObjectRef::Primary(ObjectRef::Index(index_slot)),
                false,
                false,
            )?;
        }
    }
    Ok(())
}

fn push_view_drop_dependents(
    _storage: &Storage,
    slot: usize,
    parent_is_normal: bool,
    output: &mut ArenaList<'_, DroppedObject>,
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
    output: &mut ArenaList<'_, DroppedObject>,
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
            for constraint in domain.constraint_identities() {
                push_drop(
                    output,
                    count,
                    EventObjectRef::DomainConstraint {
                        domain: domain_ref,
                        constraint: constraint.expect("admitted domain constraint has an identity"),
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
    before: &BeforeDdl<'_>,
    reference: ObjectRef,
    parent_is_normal: bool,
    output: &mut ArenaList<'_, DroppedObject>,
    count: &mut usize,
) -> Result<(), SqlError> {
    match reference {
        ObjectRef::Table(slot) => {
            push_table_drop_dependents(storage, txid, slot, None, before.indexes, output, count)
        }
        ObjectRef::MaterializedView(slot) => push_table_drop_dependents(
            storage,
            txid,
            storage.matview_table(slot),
            Some((slot, parent_is_normal)),
            before.indexes,
            output,
            count,
        ),
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
    pub before: &'b BeforeDdl<'a>,
    pub undo: &'b [DdlUndo],
    pub undo_origins: &'b [u32],
    pub origin: u32,
    pub in_extension: bool,
}

pub(crate) struct EventGraphs<'a> {
    pub commands: &'a [DdlCommand],
    pub drops: &'a [DroppedObject],
}

/// PostgreSQL collects index addresses across internal commits and calls the
/// outer end hook once; pg_event_trigger_ddl_commands resolves current names
/// and omits addresses that have since disappeared (REL_18_6 event_trigger.c).
pub(crate) fn completed_reindex_graphs<'a>(
    storage: &Storage,
    txid: u32,
    replacements: &[crate::storage::IndexReplacement],
    tag: &str,
    in_extension: bool,
    arena: &'a Arena,
) -> Result<EventGraphs<'a>, SqlError> {
    let mut commands = ArenaList::new(arena);
    for replacement in replacements {
        let Some(index) = storage.index_visible_to(replacement.new_slot, txid) else {
            continue;
        };
        if index.created_at != replacement.new_created_at {
            continue;
        }
        commands
            .push(DdlCommand {
                reference: CommandReference::ReindexedIndex {
                    slot: replacement.new_slot,
                    identity: crate::storage::RelationIdentity::parse(replacement.new_created_at)
                        .expect("completed reindex has an admitted relation identity"),
                },
                command_tag: StackStr::from_str(tag),
                in_extension,
            })
            .map_err(|_| graph_full())?;
    }
    Ok(EventGraphs {
        commands: commands.as_slice(),
        drops: &[],
    })
}

pub(crate) fn collect<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    tag: &str,
    changes: CollectChanges<'_, '_>,
    arena: &'a Arena,
) -> Result<EventGraphs<'a>, SqlError> {
    let CollectChanges {
        before,
        undo,
        undo_origins,
        origin,
        in_extension,
    } = changes;
    let mut commands = ArenaList::new(arena);
    let mut drops = ArenaList::new(arena);
    let mut command_count = 0;
    let mut drop_count = 0;
    if let Some(object_type) = utility_command_object_type(statement) {
        push_command(
            &mut commands,
            &mut command_count,
            EventObjectRef::Utility(StackStr::from_str(object_type)),
            tag,
            in_extension,
        )?;
    }
    for original_pass in [true, false] {
        for (entry, &entry_origin) in undo.iter().cloned().zip(undo_origins) {
            if entry_origin != origin {
                continue;
            }
            let Some((reference, Mutation::Drop)) = mutation(storage, entry) else {
                continue;
            };
            // A dropped materialized view reports itself, not its backing table.
            if let ObjectRef::Table(table) = reference
                && undo
                    .iter()
                    .cloned()
                    .zip(undo_origins)
                    .any(|(other, &other_origin)| {
                        other_origin == origin
                            && matches!(
                                mutation(storage, other),
                                Some((ObjectRef::MaterializedView(matview), Mutation::Drop))
                                    if usize::from(storage.matview(matview).backing_table) == table
                            )
                    })
            {
                continue;
            }
            let original = before.original_drops.contains(&reference);
            if original != original_pass {
                continue;
            }
            push_drop(
                &mut drops,
                &mut drop_count,
                EventObjectRef::Primary(reference),
                original,
                !original,
            )?;
            push_drop_dependents(
                storage,
                txid,
                before,
                reference,
                !original,
                &mut drops,
                &mut drop_count,
            )?;
        }
    }
    push_alter_table_drops(
        statement,
        before,
        storage,
        txid,
        &mut drops,
        &mut drop_count,
    )?;
    for (reference, normal) in before.dependent_drops.iter().copied() {
        push_drop_once(&mut drops, &mut drop_count, reference, false, normal)?;
    }
    let owner_target = match statement {
        Stmt::AlterOwner { kind, name, .. } => {
            super::exec::alter_owner_target(storage, txid, *kind, name)?.and_then(access_reference)
        }
        Stmt::AlterForeignServer {
            name,
            action: crate::sql::ast::AlterForeignServerAction::Owner(_),
        } => storage
            .foreign_server(name, txid)
            .map(|(slot, _)| ObjectRef::ForeignServer(slot)),
        Stmt::AlterForeignDataWrapper {
            name,
            action: crate::sql::ast::AlterForeignDataWrapperAction::Owner(_),
        } => storage
            .foreign_wrapper(name, txid)
            .map(|(slot, _)| ObjectRef::ForeignDataWrapper(slot)),
        _ => None,
    };
    let mut seen_commands = ArenaList::new(arena);
    for (entry, &entry_origin) in undo.iter().cloned().zip(undo_origins) {
        if entry_origin != origin {
            continue;
        }
        if let DdlUndo::CommentSet { slot, .. } = entry {
            // Removing an object's comment during DROP is bookkeeping, not
            // an explicit COMMENT command on a still-visible object.
            if !matches!(statement, Stmt::Comment { .. }) {
                continue;
            }
            let reference = comment_reference(storage, txid, slot as usize, statement)?;
            if seen_commands.as_slice().contains(&reference) {
                continue;
            }
            seen_commands.push(reference).map_err(|_| graph_full())?;
            push_command(
                &mut commands,
                &mut command_count,
                reference,
                tag,
                in_extension,
            )?;
            continue;
        }
        let Some((reference, mutation)) = mutation(storage, entry) else {
            continue;
        };
        if mutation != Mutation::Drop {
            // Cascaded owner updates do not add commands beyond the named root.
            if matches!(statement, Stmt::AlterOwner { .. }) && owner_target != Some(reference) {
                continue;
            }
            if let Stmt::AlterTable(_) = statement
                && let Some((root, _)) = before.altered_table.as_ref()
                && matches!(reference, ObjectRef::Table(slot) if slot != *root)
            {
                continue;
            }
            let command_reference = EventObjectRef::Primary(reference);
            if seen_commands.as_slice().contains(&command_reference) {
                continue;
            }
            seen_commands
                .push(command_reference)
                .map_err(|_| graph_full())?;
            push_command(
                &mut commands,
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
                    &mut commands,
                    &mut command_count,
                    in_extension,
                )?;
            }
        }
    }
    // A successful OWNER command is observable even when ownership is unchanged.
    // PostgreSQL REL_18_6 src/backend/tcop/utility.c collects its returned address.
    if let Some(reference) = owner_target {
        let reference = EventObjectRef::Primary(reference);
        if !seen_commands.as_slice().contains(&reference) {
            push_command(
                &mut commands,
                &mut command_count,
                reference,
                tag,
                in_extension,
            )?;
        }
    }
    debug_assert_eq!(command_count, commands.len());
    debug_assert_eq!(drop_count, drops.len());
    Ok(EventGraphs {
        commands: commands.as_slice(),
        drops: drops.as_slice(),
    })
}
