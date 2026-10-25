//! Arena-backed indexes and output for built-in catalog joins.

use super::views::columns;
use super::{Arena, Datum, SqlError, SynthTable, arena_full, finish, row, schema};
use crate::sql::eval::sqlstate;
use crate::sql_err;

pub(super) type Key = (u32, i32);

pub(super) struct Index<'a> {
    pub(super) table: SynthTable<'a>,
    entries: &'a [(Key, usize)],
}

impl<'a> Index<'a> {
    pub(super) fn new(
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

    pub(super) fn oid(table: SynthTable<'a>, arena: &'a Arena) -> Result<Self, SqlError> {
        let [oid] = columns(&table, ["oid"]);
        Self::new(table, arena, move |values| {
            Ok(Some((integer(values[oid])? as u32, 0)))
        })
    }

    pub(super) fn matching(&self, key: Key) -> impl Iterator<Item = &'a [Datum<'a>]> + '_ {
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

    pub(super) fn optional(&self, key: Option<Key>) -> impl Iterator<Item = &'a [Datum<'a>]> + '_ {
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

    pub(super) fn left(
        &self,
        key: Option<Key>,
    ) -> impl Iterator<Item = Option<&'a [Datum<'a>]>> + '_ {
        let missing = self.optional(key).next().is_none();
        self.optional(key).map(Some).chain(missing.then_some(None))
    }
}

#[derive(Clone, Copy)]
pub(super) struct CatalogRow<'a> {
    pub(super) definition: &'a crate::storage::TableDef,
    pub(super) values: &'a [Datum<'a>],
}

impl<'a> CatalogRow<'a> {
    pub(super) fn get(self, field: &str) -> Result<Datum<'a>, SqlError> {
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
    pub(super) fn integer(self, field: &str) -> Result<i32, SqlError> {
        integer(self.get(field)?)
    }
    pub(super) fn string(self, field: &str) -> Result<&'a str, SqlError> {
        string(self.get(field)?)
    }
    pub(super) fn character(self, field: &str) -> Result<u8, SqlError> {
        character(self.get(field)?)
    }
    pub(super) fn boolean(self, field: &str) -> Result<bool, SqlError> {
        boolean(self.get(field)?)
    }
}

pub(super) struct NamespaceJoin<'index, 'a> {
    objects: &'index Index<'a>,
    namespaces: &'index Index<'a>,
    namespace: usize,
}

impl<'index, 'a> NamespaceJoin<'index, 'a> {
    pub(super) fn new(
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

    pub(super) fn joined(
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

    pub(super) fn left<'join>(
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

#[derive(Clone, Copy)]
struct OutputNode<'a> {
    values: &'a [Datum<'a>],
    previous: Option<&'a OutputNode<'a>>,
}

pub(super) struct Output<'a> {
    pub(super) arena: &'a Arena,
    last: Option<&'a OutputNode<'a>>,
    count: usize,
}

impl<'a> Output<'a> {
    pub(super) fn new(arena: &'a Arena) -> Self {
        Self {
            arena,
            last: None,
            count: 0,
        }
    }

    pub(super) fn push(&mut self, values: &[Datum<'a>]) -> Result<(), SqlError> {
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

    pub(super) fn finish(self, name: &str, information: bool) -> Result<SynthTable<'a>, SqlError> {
        self.finish_rows(name, information, false)
    }

    pub(super) fn finish_distinct_names(self, name: &str) -> Result<SynthTable<'a>, SqlError> {
        self.finish_rows(name, true, true)
    }

    fn finish_rows(
        self,
        name: &str,
        information: bool,
        distinct_names: bool,
    ) -> Result<SynthTable<'a>, SqlError> {
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
        let mut count = rows.len();
        if distinct_names {
            for values in rows.iter() {
                for value in *values {
                    string(*value)?;
                }
            }
            rows.sort_unstable_by(|left, right| {
                left.iter()
                    .map(|value| string(*value).expect("validated name"))
                    .cmp(
                        right
                            .iter()
                            .map(|value| string(*value).expect("validated name")),
                    )
            });
            count = 0;
            for at in 0..rows.len() {
                if count == 0 || rows[at] != rows[count - 1] {
                    rows[count] = rows[at];
                    count += 1;
                }
            }
        }
        finish(
            schema::require(name, information),
            &rows[..count],
            self.arena,
        )
    }
}

pub(super) fn integer(value: Datum<'_>) -> Result<i32, SqlError> {
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

pub(super) fn string(value: Datum<'_>) -> Result<&str, SqlError> {
    match value {
        Datum::Text(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "catalog field is not text"
        )),
    }
}

pub(super) fn character(value: Datum<'_>) -> Result<u8, SqlError> {
    match value {
        Datum::Char(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "catalog field is not char"
        )),
    }
}

pub(super) fn boolean(value: Datum<'_>) -> Result<bool, SqlError> {
    match value {
        Datum::Bool(value) => Ok(value),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "catalog field is not boolean"
        )),
    }
}
