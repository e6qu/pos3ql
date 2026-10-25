//! Projections of selected base catalogs for information_schema.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql

use super::views::{Base, Read, columns};
use super::{Arena, Datum, SqlError, Storage, SynthTable, arena_full, finish, row, schema, text};
use crate::sql::eval::{CatalogAccess, sqlstate};
use crate::{sql_err, stack_format};

type Key = (u32, i32);

struct Index<'a> {
    table: SynthTable<'a>,
    entries: &'a [(Key, usize)],
}

impl<'a> Index<'a> {
    fn new(
        table: SynthTable<'a>,
        arena: &'a Arena,
        key: impl Fn(&[Datum<'a>]) -> Result<Option<Key>, SqlError>,
    ) -> Result<Self, SqlError> {
        let mut count = 0;
        for values in table.rows {
            count += usize::from(key(values)?.is_some());
        }
        let mut selected = table
            .rows
            .iter()
            .enumerate()
            .filter_map(|(position, values)| {
                key(values)
                    .expect("immutable catalog index keys were validated")
                    .map(|key| (key, position))
            });
        let entries = arena
            .alloc_slice_with(count, |_| {
                selected.next().expect("counted catalog index row")
            })
            .map_err(|_| arena_full())?;
        entries.sort_unstable_by_key(|(key, position)| (*key, *position));
        Ok(Self { table, entries })
    }

    fn oid(table: SynthTable<'a>, arena: &'a Arena) -> Result<Self, SqlError> {
        let [oid] = columns(&table, ["oid"]);
        Self::new(table, arena, move |values| {
            Ok(Some((integer(values[oid])? as u32, 0)))
        })
    }

    fn matching(&self, key: Key) -> impl Iterator<Item = &'a [Datum<'a>]> + '_ {
        let begin = self
            .entries
            .partition_point(|(candidate, _)| *candidate < key);
        let end = self
            .entries
            .partition_point(|(candidate, _)| *candidate <= key);
        self.entries[begin..end]
            .iter()
            .map(|(_, position)| self.table.rows[*position])
    }

    fn optional(&self, key: Option<Key>) -> impl Iterator<Item = &'a [Datum<'a>]> + '_ {
        let (begin, end) = match key {
            Some(key) => (
                self.entries
                    .partition_point(|(candidate, _)| *candidate < key),
                self.entries
                    .partition_point(|(candidate, _)| *candidate <= key),
            ),
            None => (0, 0),
        };
        self.entries[begin..end]
            .iter()
            .map(|(_, position)| self.table.rows[*position])
    }

    fn left(&self, key: Option<Key>) -> impl Iterator<Item = Option<&'a [Datum<'a>]>> + '_ {
        let missing = self.optional(key).next().is_none();
        self.optional(key).map(Some).chain(missing.then_some(None))
    }
}

#[derive(Clone, Copy)]
struct CatalogRow<'a> {
    definition: &'a crate::storage::TableDef,
    values: &'a [Datum<'a>],
}

impl<'a> CatalogRow<'a> {
    fn get(self, field: &str) -> Result<Datum<'a>, SqlError> {
        let position = self.definition.column_index(field).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog source is missing field {}",
                field
            )
        })?;
        self.values.get(position).copied().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog source row is missing field {}",
                field
            )
        })
    }
    fn integer(self, field: &str) -> Result<i32, SqlError> {
        integer(self.get(field)?)
    }
    fn string(self, field: &str) -> Result<&'a str, SqlError> {
        string(self.get(field)?)
    }
    fn character(self, field: &str) -> Result<u8, SqlError> {
        character(self.get(field)?)
    }
    fn boolean(self, field: &str) -> Result<bool, SqlError> {
        boolean(self.get(field)?)
    }
}

struct NamespaceJoin<'index, 'a> {
    objects: &'index Index<'a>,
    namespaces: &'index Index<'a>,
    namespace: usize,
}

impl<'index, 'a> NamespaceJoin<'index, 'a> {
    fn new(
        objects: &'index Index<'a>,
        namespaces: &'index Index<'a>,
        namespace: &str,
    ) -> Result<Self, SqlError> {
        let [namespace] = columns(&objects.table, [namespace]);
        for values in objects.table.rows {
            integer(values[namespace])?;
        }
        Ok(Self {
            objects,
            namespaces,
            namespace,
        })
    }

    fn joined(
        &self,
        key: Option<Key>,
    ) -> impl Iterator<Item = (CatalogRow<'a>, CatalogRow<'a>)> + '_ {
        self.objects.optional(key).flat_map(move |values| {
            let namespace = integer(values[self.namespace])
                .expect("immutable namespace identifiers were validated");
            self.namespaces
                .matching((namespace as u32, 0))
                .map(move |names| {
                    (
                        CatalogRow {
                            definition: self.objects.table.def,
                            values,
                        },
                        CatalogRow {
                            definition: self.namespaces.table.def,
                            values: names,
                        },
                    )
                })
        })
    }

    fn left<'join>(
        &'join self,
        key: Option<Key>,
        include: impl Fn(CatalogRow<'a>, CatalogRow<'a>) -> bool + Copy + 'join,
    ) -> impl Iterator<Item = Option<(CatalogRow<'a>, CatalogRow<'a>)>> + 'join {
        let missing = !self
            .joined(key)
            .any(|(object, namespace)| include(object, namespace));
        self.joined(key)
            .filter(move |(object, namespace)| include(*object, *namespace))
            .map(Some)
            .chain(missing.then_some(None))
    }
}

pub(super) struct ScalarMetadata {
    pub character_length: Option<i32>,
    pub octet_length: Option<i32>,
    pub precision: Option<i32>,
    pub radix: Option<i32>,
    pub scale: Option<i32>,
    pub datetime_precision: Option<i32>,
}

impl ScalarMetadata {
    pub(super) fn new(oid: i32, modifier: i32, character_bytes: i32) -> Result<Self, SqlError> {
        let overflow = || {
            sql_err!(
                sqlstate::NUMERIC_OUT_OF_RANGE,
                "information schema type modifier overflow"
            )
        };
        let character_length = match (oid, modifier) {
            (_, -1) => None,
            (1042 | 1043, modifier) => Some(modifier.checked_sub(4).ok_or_else(overflow)?),
            (1560 | 1562, modifier) => Some(modifier),
            _ => None,
        };
        let octet_length = if matches!(oid, 25 | 1042 | 1043) {
            if modifier == -1 {
                Some(1 << 30)
            } else {
                character_length
                    .map(|length| length.checked_mul(character_bytes).ok_or_else(overflow))
                    .transpose()?
            }
        } else {
            None
        };
        let numeric_modifier = if oid == 1700 && modifier != -1 {
            Some(modifier.checked_sub(4).ok_or_else(overflow)?)
        } else {
            None
        };
        let precision = match oid {
            21 => Some(16),
            23 => Some(32),
            20 => Some(64),
            700 => Some(24),
            701 => Some(53),
            1700 => numeric_modifier.map(|value| (value >> 16) & 0xffff),
            _ => None,
        };
        let radix = match oid {
            21 | 23 | 20 | 700 | 701 => Some(2),
            1700 => Some(10),
            _ => None,
        };
        let scale = match oid {
            21 | 23 | 20 => Some(0),
            1700 => numeric_modifier.map(|value| value & 0xffff),
            _ => None,
        };
        let datetime_precision = match oid {
            1082 => Some(0),
            1083 | 1114 | 1184 | 1266 => Some(if modifier < 0 { 6 } else { modifier }),
            1186 => Some(if modifier < 0 || modifier & 0xffff == 0xffff {
                6
            } else {
                modifier & 0xffff
            }),
            _ => None,
        };
        Ok(Self {
            character_length,
            octet_length,
            precision,
            radix,
            scale,
            datetime_precision,
        })
    }
}

#[derive(Clone, Copy)]
struct OutputNode<'a> {
    values: &'a [Datum<'a>],
    previous: Option<&'a OutputNode<'a>>,
}

struct Output<'a> {
    arena: &'a Arena,
    last: Option<&'a OutputNode<'a>>,
    count: usize,
}

impl<'a> Output<'a> {
    fn new(arena: &'a Arena) -> Self {
        Self {
            arena,
            last: None,
            count: 0,
        }
    }

    fn push(&mut self, values: &[Datum<'a>]) -> Result<(), SqlError> {
        let count = self.count.checked_add(1).ok_or_else(arena_full)?;
        let values = row(values, self.arena)?;
        let node = self
            .arena
            .alloc(OutputNode {
                values,
                previous: self.last,
            })
            .map_err(|_| arena_full())?;
        self.last = Some(node);
        self.count = count;
        Ok(())
    }

    fn finish(self, name: &str) -> Result<SynthTable<'a>, SqlError> {
        let rows = self
            .arena
            .alloc_slice_with(self.count, |_| &[] as &[Datum])
            .map_err(|_| arena_full())?;
        let mut current = self.last;
        for target in rows.iter_mut().rev() {
            let node = current.expect("each counted output row has an arena node");
            *target = node.values;
            current = node.previous;
        }
        finish(schema::require(name, true), rows, self.arena)
    }
}

fn integer(value: Datum<'_>) -> Result<i32, SqlError> {
    match value {
        Datum::Int2(value) => Ok(i32::from(value)),
        Datum::Int4(value) => Ok(value),
        Datum::Oid(value) => Ok(value as i32),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "catalog field is not an integer"
        )),
    }
}

fn string(value: Datum<'_>) -> Result<&str, SqlError> {
    match value {
        Datum::Text(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "catalog field is not text"
        )),
    }
}

fn character(value: Datum<'_>) -> Result<u8, SqlError> {
    match value {
        Datum::Char(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "catalog field is not char"
        )),
    }
}

fn boolean(value: Datum<'_>) -> Result<bool, SqlError> {
    match value {
        Datum::Bool(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "catalog field is not boolean"
        )),
    }
}

fn owner_is_enabled(storage: &Storage, transaction: u32, owner: i32) -> bool {
    let Some(actor) = storage.current_role_slot(transaction) else {
        return false;
    };
    if storage.role(actor).attributes_to(transaction).superuser {
        return true;
    }
    let owner = if owner == super::PG_DATABASE_OWNER_OID {
        let database = storage.current_database_name(transaction);
        storage
            .resolve_access_object(
                crate::storage::AccessClass::Database,
                "",
                database.as_str(),
                transaction,
            )
            .map(|object| storage.object_owner(object, transaction))
    } else {
        storage.role_slot_by_oid(owner, transaction)
    };
    owner.is_some_and(|owner| storage.role_is_enabled(owner as u16, transaction))
}

fn relation_is_updatable(
    storage: &Storage,
    transaction: u32,
    name: &str,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let (qualifier, name) = crate::sql::query::split_catalog_name(name);
    match storage.resolve_relation(qualifier, name, transaction) {
        Some(crate::storage::ResolvedRelation::View(slot)) => {
            let view = storage.view(slot);
            crate::sql::query::view_is_auto_updatable(
                storage,
                view.schema_for(transaction).as_str(),
                view.name_for(transaction).as_str(),
                transaction,
                arena,
            )
        }
        Some(crate::storage::ResolvedRelation::Table(table)) => {
            let Some((_, binding)) = storage.foreign_table(table as u16, transaction) else {
                return Ok(true);
            };
            if let Some(value) = binding.options.get("updatable") {
                return crate::sql::eval::parse_bool(value);
            }
            let server = storage
                .foreign_server_by_slot(binding.server as usize, transaction)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "foreign table references a missing foreign server"
                    )
                })?;
            match server.options.get("updatable") {
                Some(value) => crate::sql::eval::parse_bool(value),
                None => Ok(true),
            }
        }
        Some(crate::storage::ResolvedRelation::Catalog) => Ok(false),
        None => Ok(false),
    }
}

pub(super) fn tables<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let classes = read.base(storage, Base::Class, transaction, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let types = Index::oid(read.base(storage, Base::Type, transaction, arena)?, arena)?;
    let [oid, namespace, name, kind, owner, typed] = columns(
        &classes,
        [
            "oid",
            "relnamespace",
            "relname",
            "relkind",
            "relowner",
            "reloftype",
        ],
    );
    let [namespace_oid, namespace_name] = columns(&namespaces.table, ["oid", "nspname"]);
    let [type_namespace, type_name] = columns(&types.table, ["typnamespace", "typname"]);
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let mut output = Output::new(arena);
    for class in classes.rows {
        let kind = character(class[kind])?;
        if !matches!(kind, b'r' | b'p' | b'v' | b'f') {
            continue;
        }
        let relation_oid = integer(class[oid])?;
        let current_name = catalog.relname(relation_oid, arena)?;
        let visible = owner_is_enabled(storage, transaction, integer(class[owner])?)
            || if let Some(name) = current_name {
                catalog.has_table_privilege(
                    None,
                    name,
                    "SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER",
                )? == Some(true)
                    || catalog.has_any_column_privilege(
                        None,
                        name,
                        "SELECT, INSERT, UPDATE, REFERENCES",
                    )? == Some(true)
            } else {
                false
            };
        if !visible {
            continue;
        }
        let insertable = matches!(kind, b'r' | b'p')
            || if let Some(name) = current_name {
                relation_is_updatable(storage, transaction, name, arena)?
            } else {
                false
            };
        for namespace in namespaces.matching((integer(class[namespace])? as u32, 0)) {
            let namespace_oid = integer(namespace[namespace_oid])?;
            if catalog.is_other_temporary_namespace(namespace_oid) {
                continue;
            }
            let table_type = if namespace_oid == catalog.current_temporary_namespace_oid() {
                "LOCAL TEMPORARY"
            } else {
                match kind {
                    b'r' | b'p' => "BASE TABLE",
                    b'v' => "VIEW",
                    b'f' => "FOREIGN",
                    _ => unreachable!(),
                }
            };
            let mut has_type = false;
            for ty in types.matching((integer(class[typed])? as u32, 0)) {
                for type_namespace in namespaces.matching((integer(ty[type_namespace])? as u32, 0))
                {
                    has_type = true;
                    output.push(&[
                        database,
                        namespace[namespace_name],
                        class[name],
                        text(table_type, arena)?,
                        Datum::Null,
                        Datum::Null,
                        database,
                        type_namespace[namespace_name],
                        ty[type_name],
                        text(if insertable { "YES" } else { "NO" }, arena)?,
                        text("YES", arena)?,
                        Datum::Null,
                    ])?;
                }
            }
            if !has_type {
                output.push(&[
                    database,
                    namespace[namespace_name],
                    class[name],
                    text(table_type, arena)?,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    text(if insertable { "YES" } else { "NO" }, arena)?,
                    text("NO", arena)?,
                    Datum::Null,
                ])?;
            }
        }
    }
    output.finish("tables")
}

struct ColumnSources<'a> {
    attributes: SynthTable<'a>,
    classes: Index<'a>,
    namespaces: Index<'a>,
    types: Index<'a>,
    defaults: Index<'a>,
    collations: Index<'a>,
    dependencies: Index<'a>,
    sequences: Index<'a>,
}

impl<'a> ColumnSources<'a> {
    fn new(
        storage: &Storage,
        transaction: u32,
        read: Read,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let attributes = read.base(storage, Base::Attribute, transaction, arena)?;
        let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
        let namespaces = Index::oid(
            read.base(storage, Base::Namespace, transaction, arena)?,
            arena,
        )?;
        let types = Index::oid(read.base(storage, Base::Type, transaction, arena)?, arena)?;
        let source = read.base(storage, Base::Defaults, transaction, arena)?;
        let definition = source.def;
        let defaults = Index::new(source, arena, move |values| {
            let value = CatalogRow { definition, values };
            Ok(Some((
                value.integer("adrelid")? as u32,
                value.integer("adnum")?,
            )))
        })?;
        let collations = Index::oid(
            read.base(storage, Base::Collation, transaction, arena)?,
            arena,
        )?;
        let source = read.base(storage, Base::Dependencies, transaction, arena)?;
        let definition = source.def;
        let dependencies = Index::new(source, arena, move |values| {
            let value = CatalogRow { definition, values };
            if value.integer("classid")? != super::PG_CLASS_OID
                || value.integer("refclassid")? != super::PG_CLASS_OID
                || value.character("deptype")? != b'i'
            {
                return Ok(None);
            }
            value.integer("objid")?;
            Ok(Some((
                value.integer("refobjid")? as u32,
                value.integer("refobjsubid")?,
            )))
        })?;
        let source = read.base(storage, Base::Sequence, transaction, arena)?;
        let definition = source.def;
        let sequences = Index::new(source, arena, move |values| {
            let value = CatalogRow { definition, values };
            Ok(Some((value.integer("seqrelid")? as u32, 0)))
        })?;
        Ok(Self {
            attributes,
            classes,
            namespaces,
            types,
            defaults,
            collations,
            dependencies,
            sequences,
        })
    }

    fn identity_sequences(&self, key: Key) -> impl Iterator<Item = Option<CatalogRow<'a>>> + '_ {
        let [object] = columns(&self.dependencies.table, ["objid"]);
        let joined = || {
            self.dependencies.matching(key).flat_map(move |dependency| {
                let oid = integer(dependency[object])
                    .expect("immutable sequence dependency identifiers were validated");
                self.sequences
                    .matching((oid as u32, 0))
                    .map(move |values| CatalogRow {
                        definition: self.sequences.table.def,
                        values,
                    })
            })
        };
        let missing = joined().next().is_none();
        joined().map(Some).chain(missing.then_some(None))
    }
}

struct ColumnVersion<'a> {
    attribute: CatalogRow<'a>,
    class: CatalogRow<'a>,
    namespace: CatalogRow<'a>,
    ty: (CatalogRow<'a>, CatalogRow<'a>),
    base: Option<(CatalogRow<'a>, CatalogRow<'a>)>,
    default: Option<CatalogRow<'a>>,
    collation: Option<(CatalogRow<'a>, CatalogRow<'a>)>,
    sequence: Option<CatalogRow<'a>>,
    updatable: bool,
}

fn data_type<'a>(
    ty: CatalogRow<'a>,
    namespace: CatalogRow<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if ty.integer("typelem")? != 0 && ty.integer("typlen")? == -1 {
        return text("ARRAY", arena);
    }
    if namespace.string("nspname")? != "pg_catalog" {
        return text("USER-DEFINED", arena);
    }
    match crate::sql::exec::coltype_of_oid(ty.integer("oid")?) {
        Some(ctype) => text(ctype.name(), arena),
        None => ty.get("typname"),
    }
}

fn sequence_text<'a>(
    sequence: Option<CatalogRow<'a>>,
    field: &str,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    match sequence {
        None => Ok(Datum::Null),
        Some(sequence) => match sequence.get(field)? {
            Datum::Int8(value) => text(stack_format!(32, "{}", value).as_str(), arena),
            _ => Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "sequence field is not bigint"
            )),
        },
    }
}

fn emit_column<'a>(
    storage: &Storage,
    transaction: u32,
    version: ColumnVersion<'a>,
    output: &mut Output<'a>,
) -> Result<(), SqlError> {
    let ColumnVersion {
        attribute,
        class,
        namespace,
        ty: (ty, ty_namespace),
        base,
        default,
        collation,
        sequence,
        updatable,
    } = version;
    let arena = output.arena;
    let domain = ty.character("typtype")? == b'd';
    let true_oid = if domain {
        ty.integer("typbasetype")?
    } else {
        attribute.integer("atttypid")?
    };
    let modifier = if domain {
        ty.integer("typtypmod")?
    } else {
        attribute.integer("atttypmod")?
    };
    let scalar = ScalarMetadata::new(
        true_oid,
        modifier,
        storage
            .current_database_encoding(transaction)
            .maximum_character_bytes(),
    )?;
    let interval_type = if true_oid == 1186 {
        super::information_schema_interval_type(crate::sql::types::TypeMod::decode(
            crate::sql::types::ColType::Interval,
            modifier,
        ))
    } else {
        None
    };
    let generated = attribute.character("attgenerated")? != 0;
    let identity = attribute.character("attidentity")?;
    let expression = match default {
        Some(default) => default.get("adbin")?,
        None => Datum::Null,
    };
    let database = storage.current_database_name(transaction);
    let database = text(database.as_str(), arena)?;
    let (underlying, underlying_namespace) = base.unwrap_or((ty, ty_namespace));
    let data_type = if domain && base.is_none() {
        text("USER-DEFINED", arena)?
    } else {
        data_type(underlying, underlying_namespace, arena)?
    };
    output.push(&[
        database,
        namespace.get("nspname")?,
        class.get("relname")?,
        attribute.get("attname")?,
        Datum::Int4(attribute.integer("attnum")?),
        if generated { Datum::Null } else { expression },
        text(
            if attribute.boolean("attnotnull")? || (domain && ty.boolean("typnotnull")?) {
                "NO"
            } else {
                "YES"
            },
            arena,
        )?,
        data_type,
        scalar.character_length.map_or(Datum::Null, Datum::Int4),
        scalar.octet_length.map_or(Datum::Null, Datum::Int4),
        scalar.precision.map_or(Datum::Null, Datum::Int4),
        scalar.radix.map_or(Datum::Null, Datum::Int4),
        scalar.scale.map_or(Datum::Null, Datum::Int4),
        scalar.datetime_precision.map_or(Datum::Null, Datum::Int4),
        match interval_type {
            Some(value) => text(value.as_str(), arena)?,
            None => Datum::Null,
        },
        Datum::Null,
        Datum::Null,
        Datum::Null,
        Datum::Null,
        if collation.is_some() {
            database
        } else {
            Datum::Null
        },
        match collation {
            Some((_, namespace)) => namespace.get("nspname")?,
            None => Datum::Null,
        },
        match collation {
            Some((collation, _)) => collation.get("collname")?,
            None => Datum::Null,
        },
        if domain { database } else { Datum::Null },
        if domain {
            ty_namespace.get("nspname")?
        } else {
            Datum::Null
        },
        if domain {
            ty.get("typname")?
        } else {
            Datum::Null
        },
        database,
        underlying_namespace.get("nspname")?,
        underlying.get("typname")?,
        Datum::Null,
        Datum::Null,
        Datum::Null,
        Datum::Null,
        text(
            stack_format!(32, "{}", attribute.integer("attnum")?).as_str(),
            arena,
        )?,
        text("NO", arena)?,
        text(
            if matches!(identity, b'a' | b'd') {
                "YES"
            } else {
                "NO"
            },
            arena,
        )?,
        match identity {
            b'a' => text("ALWAYS", arena)?,
            b'd' => text("BY DEFAULT", arena)?,
            _ => Datum::Null,
        },
        sequence_text(sequence, "seqstart", arena)?,
        sequence_text(sequence, "seqincrement", arena)?,
        sequence_text(sequence, "seqmax", arena)?,
        sequence_text(sequence, "seqmin", arena)?,
        text(
            if sequence
                .map(|sequence| sequence.boolean("seqcycle"))
                .transpose()?
                == Some(true)
            {
                "YES"
            } else {
                "NO"
            },
            arena,
        )?,
        text(if generated { "ALWAYS" } else { "NEVER" }, arena)?,
        if generated { expression } else { Datum::Null },
        text(if updatable { "YES" } else { "NO" }, arena)?,
    ])
}

pub(super) fn columns_view<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let sources = ColumnSources::new(storage, transaction, read, arena)?;
    let types = NamespaceJoin::new(&sources.types, &sources.namespaces, "typnamespace")?;
    let collations = NamespaceJoin::new(&sources.collations, &sources.namespaces, "collnamespace")?;
    let [collation_name] = columns(&sources.collations.table, ["collname"]);
    let [namespace_name] = columns(&sources.namespaces.table, ["nspname"]);
    for values in sources.collations.table.rows {
        string(values[collation_name])?;
    }
    for values in sources.namespaces.table.rows {
        string(values[namespace_name])?;
    }
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let mut output = Output::new(arena);
    for values in sources.attributes.rows {
        let attribute = CatalogRow {
            definition: sources.attributes.def,
            values,
        };
        let position = attribute.integer("attnum")?;
        if position <= 0 || attribute.boolean("attisdropped")? {
            continue;
        }
        let number = i16::try_from(position).map_err(|_| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog attribute number is out of range"
            )
        })?;
        let relation = attribute.integer("attrelid")?;
        let key = (relation as u32, position);
        for values in sources.classes.matching((relation as u32, 0)) {
            let class = CatalogRow {
                definition: sources.classes.table.def,
                values,
            };
            let kind = class.character("relkind")?;
            if !matches!(kind, b'r' | b'p' | b'v' | b'f') {
                continue;
            }
            let current_name = catalog.relname(relation, arena)?;
            let visible = owner_is_enabled(storage, transaction, class.integer("relowner")?)
                || if let Some(name) = current_name {
                    catalog.has_column_privilege(
                        None,
                        name,
                        crate::sql::eval::PrivilegeColumn::Number(number),
                        "SELECT, INSERT, UPDATE, REFERENCES",
                    )? == Some(true)
                } else {
                    false
                };
            if !visible {
                continue;
            }
            let updatable = matches!(kind, b'r' | b'p')
                || if let Some(name) = current_name {
                    relation_is_updatable(storage, transaction, name, arena)?
                } else {
                    false
                };
            for values in sources
                .namespaces
                .matching((class.integer("relnamespace")? as u32, 0))
            {
                let namespace = CatalogRow {
                    definition: sources.namespaces.table.def,
                    values,
                };
                if catalog.is_other_temporary_namespace(namespace.integer("oid")?) {
                    continue;
                }
                for (ty, ty_namespace) in
                    types.joined(Some((attribute.integer("atttypid")? as u32, 0)))
                {
                    let base = (ty.character("typtype")? == b'd')
                        .then(|| ty.integer("typbasetype"))
                        .transpose()?;
                    for base in types.left(base.map(|oid| (oid as u32, 0)), |_, _| true) {
                        for default in sources.defaults.left(Some(key)) {
                            let default = default.map(|values| CatalogRow {
                                definition: sources.defaults.table.def,
                                values,
                            });
                            for collation in collations.left(
                                Some((attribute.integer("attcollation")? as u32, 0)),
                                |collation, namespace| {
                                    !matches!(
                                        (
                                            namespace.values[namespace_name],
                                            collation.values[collation_name]
                                        ),
                                        (Datum::Text("pg_catalog"), Datum::Text("default"))
                                    )
                                },
                            ) {
                                for sequence in sources.identity_sequences(key) {
                                    emit_column(
                                        storage,
                                        transaction,
                                        ColumnVersion {
                                            attribute,
                                            class,
                                            namespace,
                                            ty: (ty, ty_namespace),
                                            base,
                                            default,
                                            collation,
                                            sequence,
                                            updatable,
                                        },
                                        &mut output,
                                    )?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    output.finish("columns")
}
