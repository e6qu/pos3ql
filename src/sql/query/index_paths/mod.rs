//! PostgreSQL 18's choice of access path for a query reading one plain
//! table: the sequential scan, each btree index's index or index-only scan
//! and the bitmap heap scan, costed as `costsize.c` costs them
//! (`cost_seqscan`, `cost_index` with `btcostestimate` and
//! `genericcostestimate`, `cost_bitmap_heap_scan`), with the selectivities
//! of `selfuncs.c` and `clausesel.c` from the statistics ANALYZE gathers,
//! and kept or discarded as `add_path` and `set_cheapest` do. EXPLAIN shows
//! the chosen path and execution returns its rows in that path's order.
//!
//! The model covers plain btree indexes of any number of key columns, and
//! restrictions that compare a column with constants (`=`, `<>`, `<`,
//! `<=`, `>`, `>=`, `IN` lists and `op ANY (array)`), test it for NULL, or
//! combine those with `OR`. A query outside it names the shape it leaves
//! out ([`Planned::Outside`]).

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, Collation, Expr, IndexAccessMethod};
use crate::sql::catalog::PlannerIndex;
use crate::sql::eval::SqlError;
use crate::sql::types::{ColType, Datum};
use crate::storage::{
    ColumnDistribution, ColumnSet, MAX_COLUMNS, Storage, TableDef, TableStatistics,
};

use super::scope::{QueryScope, ResolvedColumn};

pub(super) mod aggregate;
mod append;
mod clauses;
mod expressions;
mod extended;
mod group_keys;
mod grouping_sets;
pub(crate) mod join_query;
mod join_tree;
pub(crate) mod joins;
mod like;
mod paths;
mod predicate;

pub(crate) use expressions::{ExpressionVariables, KeyReader};
pub(crate) use group_keys::{GroupKeys, GroupVariable};

pub(crate) use aggregate::{
    AggInput, AggStrategy, AggregateCosts, aggregate_costs, cost_agg, hash_agg_sizing,
    having_quals, plain_hash_sizing, target_cost,
};
pub(crate) use append::{AppendKind, MemberPath, TreePath, member_path, member_scope};
pub(crate) use grouping_sets::{
    GroupingSet, MAX_SET_COLUMNS, SetsPlan, SetsQuery, SetsStrategy, plan_grouping_sets,
};
pub(crate) use like::like_escape;
pub(crate) use paths::scan_path;

/// Whether the table in `slot` has an index.
pub(crate) fn has_indexes(storage: &Storage, txid: u32, slot: usize) -> bool {
    let mut found = false;
    crate::sql::catalog::planner_indexes(storage, txid, slot, &mut |_| found = true);
    found
}

/// The path PostgreSQL chooses for an `UPDATE`'s or `DELETE`'s scan of its
/// target `target`, written `alias`, under `condition`: its rows' identity
/// is read too, so no index-only scan serves it.
#[expect(clippy::too_many_arguments, reason = "the write's parts are explicit")]
pub(crate) fn write_scan_path<'a>(
    storage: &'a Storage,
    txid: u32,
    target: crate::sql::ast::QualName<'a>,
    alias: Option<&'a str>,
    condition: Option<&'a Expr<'a>>,
    assignments: &[(&str, &'a Expr<'a>)],
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Planned<'a>, SqlError> {
    let (scope, shape) = write_shape(storage, txid, target, alias, assignments, arena)?;
    scan_path(storage, &scope, txid, condition, None, params, arena, shape)
}

/// The scope an `UPDATE` (with its `assignments`) or `DELETE` of `target`
/// reads, and the shape of its scan: the new values and the row identity
/// it returns.
pub(crate) fn write_shape<'a>(
    storage: &'a Storage,
    txid: u32,
    target: crate::sql::ast::QualName<'a>,
    alias: Option<&'a str>,
    assignments: &[(&str, &'a Expr<'a>)],
    arena: &'a Arena,
) -> Result<(QueryScope<'a>, QueryShape<'a>), SqlError> {
    let from = arena
        .alloc(crate::sql::ast::FromClause {
            base: super::dml_target_ref(target, alias),
            joins: &[],
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    let scope = QueryScope::resolve_schema(storage, from, txid, arena, None)?;
    let tree = scope.slots[0] != usize::MAX
        && (storage
            .table_def(scope.slots[0], txid)
            .partition
            .is_partitioned()
            || storage.relation_has_descendants(scope.slots[0], txid));
    // `ctid`, and `tableoid` naming a tree's member.
    let identity_width = if tree { 6 + 4 } else { 6 };
    let projection = write_projection(storage, txid, &scope, assignments, identity_width, arena)?;
    // The columns the new values read (`reltarget`).
    let mut read = 0u64;
    for &(_, expression) in assignments {
        expression.for_each_column_reference(&mut |qualifier, name| {
            if let Ok(ResolvedColumn::Table(0, column)) = scope.find_column(qualifier, name) {
                read |= if column < 64 { 1 << column } else { 1 << 63 };
            }
        });
    }
    let output = projection.map(|_| ColumnSet::from_low_word(read));
    Ok((
        scope,
        QueryShape {
            target: true,
            output,
            projection,
            identity_width,
            ..QueryShape::default()
        },
    ))
}

/// The target list a write's scan computes: an `UPDATE`'s new values,
/// then the row identity of `identity_width` bytes.
fn write_projection<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    assignments: &[(&str, &'a Expr<'a>)],
    identity_width: u32,
    arena: &'a Arena,
) -> Result<Option<Projection>, SqlError> {
    if scope.n != 1 || scope.slots[0] == usize::MAX {
        return Ok(None);
    }
    let Some(definition) = scope.defs.stored(0) else {
        return Ok(None);
    };
    let statistics = storage.table_statistics(scope.slots[0], txid);
    let mut projection = Projection {
        width: identity_width,
        per_tuple: 0.0,
        expression_width: identity_width,
        columns: [0; 64],
        wide: false,
        plain: true,
    };
    let costs = Costs::current();
    for &(target, expression) in assignments {
        if expression.contains_subquery() {
            return Ok(None);
        }
        // A column of the relation, or an expression coerced to the
        // assigned column's type.
        let (width, cost, column) = match expression {
            Expr::Column { qualifier, name }
                if let Ok(ResolvedColumn::Table(0, column)) =
                    scope.find_column(*qualifier, name) =>
            {
                (
                    output_width(
                        definition,
                        &statistics,
                        Some(ColumnSet::from_column(column)),
                    ),
                    0.0,
                    Some(column),
                )
            }
            _ => {
                let Some(assigned) = definition
                    .columns()
                    .iter()
                    .find(|column| column.name.as_str() == target)
                else {
                    return Ok(None);
                };
                let columns = crate::sql::exec::AliasedDefCols {
                    definition: crate::storage::RelationDefinition::from_definition(definition),
                    alias: None,
                    output_aliases: &[],
                    remote_ctid: false,
                };
                (
                    type_average_width(assigned.ctype, assigned.type_mod) as u32,
                    super::plan::qual_cost(expression, &columns, storage, txid, arena) / 2.0
                        * costs.cpu_operator,
                    None,
                )
            }
        };
        projection.width += width;
        projection.per_tuple += cost;
        match column {
            Some(column) if column < 64 => {
                projection.columns[column] += 1;
                projection.plain &= projection.columns[column] == 1;
            }
            Some(_) => projection.wide = true,
            None => {
                projection.plain = false;
                projection.expression_width += width;
            }
        }
    }
    Ok(Some(projection))
}

/// `selfuncs.h`' default selectivities.
const DEFAULT_EQ_SEL: f64 = 0.005;
const DEFAULT_INEQ_SEL: f64 = 0.333_333_333_333_333_3;
const DEFAULT_RANGE_INEQ_SEL: f64 = 0.005;
const DEFAULT_NUM_DISTINCT: f64 = 200.0;
const DEFAULT_UNK_SEL: f64 = 0.005;
const DEFAULT_NOT_UNK_SEL: f64 = 1.0 - DEFAULT_UNK_SEL;
const MAX_RESTRICTIONS: usize = 32;
const MAX_INDEXES: usize = 32;

/// The planner's cost settings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Costs {
    pub(crate) seq_page: f64,
    pub(crate) random_page: f64,
    pub(crate) cpu_tuple: f64,
    pub(crate) cpu_index_tuple: f64,
    pub(crate) cpu_operator: f64,
    pub(crate) effective_cache_size: f64,
}

impl Costs {
    fn current() -> Self {
        let value = crate::sql::guc::planner_value;
        Self {
            seq_page: value("seq_page_cost"),
            random_page: value("random_page_cost"),
            cpu_tuple: value("cpu_tuple_cost"),
            cpu_index_tuple: value("cpu_index_tuple_cost"),
            cpu_operator: value("cpu_operator_cost"),
            effective_cache_size: value("effective_cache_size"),
        }
    }
}

/// `MAXIMUM_ROWCOUNT`: the largest row estimate the planner keeps.
pub(crate) const MAXIMUM_ROWCOUNT: f64 = 1e100;

/// `clamp_row_est`: at most [`MAXIMUM_ROWCOUNT`] (also for NaN), at least
/// one, else `rint`, which rounds a half to even.
pub(crate) fn clamp_row_est(rows: f64) -> f64 {
    if rows > MAXIMUM_ROWCOUNT || rows.is_nan() {
        MAXIMUM_ROWCOUNT
    } else if rows <= 1.0 {
        1.0
    } else {
        rows.round_ties_even()
    }
}

fn clamp_probability(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

/// The relation's pages, tuples and all-visible fraction as
/// `estimate_rel_size` gives them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RelationSize {
    pub(crate) pages: f64,
    pub(crate) tuples: f64,
    pub(crate) allvisfrac: f64,
}

/// `get_typavgwidth` for a column without a statistics width.
pub(crate) fn type_average_width(ctype: ColType, type_mod: i32) -> f64 {
    let layout = ctype.physical_layout();
    if layout.length > 0 {
        return f64::from(layout.length);
    }
    // `type_maximum_size`: a length-limited character type's four bytes
    // per UTF-8 character plus the header, a bit string's bytes and its
    // header and length, and `numeric_maximum_size` of a numeric's
    // precision: its two-byte digits of four decimal digits each, the first
    // possibly holding one, after an eight-byte header.
    let maximum = match ctype {
        ColType::Varchar | ColType::Bpchar if type_mod > 4 => (type_mod - 4) * 4 + 4,
        ColType::Bit { .. } if type_mod > 0 => (type_mod + 7) / 8 + 8,
        ColType::Numeric if type_mod >= 4 => {
            let precision = ((type_mod - 4) >> 16) & 0xffff;
            8 + (precision + 6) / 4 * 2
        }
        _ => 0,
    };
    if maximum > 0 {
        if ctype == ColType::Bpchar || maximum <= 32 {
            return f64::from(maximum);
        }
        if maximum < 1000 {
            return f64::from(32 + (maximum - 32) / 2);
        }
        return f64::from(32 + (1000 - 32) / 2);
    }
    32.0
}

/// `get_rel_data_width`: each column's analyzed width, or its type's.
fn data_width(definition: &TableDef, statistics: &TableStatistics) -> f64 {
    let mut width = 0.0;
    for (column, meta) in definition.columns().iter().enumerate() {
        let analyzed = definition
            .attributes
            .number(column)
            .and_then(|attribute| statistics.column(attribute))
            .filter(|column| column.average_width > 0)
            .map(|column| f64::from(column.average_width));
        width += analyzed.unwrap_or_else(|| type_average_width(meta.ctype, meta.type_mod));
    }
    width
}

pub(crate) fn relation_size(storage: &Storage, slot: usize, txid: u32) -> RelationSize {
    let statistics = storage.table_statistics(slot, txid);
    let definition = storage.table_def(slot, txid);
    let width = data_width(&definition, &statistics);
    let density_from_width = clamp_row_est(
        // (BLCKSZ - SizeOfPageHeaderData) at the default fill factor over
        // the tuple's width, its header and line pointer: integer division.
        (8168 / (width as u64 + 28).max(1)) as f64,
    );
    let (reltuples, relpages, relallvisible) = if statistics.valid {
        (
            statistics.rows as f64,
            statistics.pages as f64,
            statistics.all_visible_pages as f64,
        )
    } else {
        (-1.0, 0.0, 0.0)
    };
    let mut curpages = if statistics.valid {
        relpages
    } else {
        (storage.planning_row_estimate(slot) as f64 / density_from_width).ceil()
    };
    if curpages < 10.0 && reltuples < 0.0 && !storage.relation_has_descendants(slot, txid) {
        curpages = 10.0;
    }
    if curpages == 0.0 {
        return RelationSize {
            pages: 0.0,
            tuples: 0.0,
            allvisfrac: 0.0,
        };
    }
    let density = if reltuples >= 0.0 && relpages > 0.0 {
        reltuples / relpages
    } else {
        density_from_width
    };
    let allvisfrac = if relallvisible == 0.0 {
        0.0
    } else if relallvisible >= curpages {
        1.0
    } else {
        relallvisible / curpages
    };
    RelationSize {
        pages: curpages,
        tuples: (density * curpages).round_ties_even(),
        allvisfrac,
    }
}

/// A btree index's pages and height, as CREATE INDEX lays out its entries:
/// leaves filled to 90%, inner pages to 70%, equal keys of a deduplicating
/// index merged into posting lists of at most 812 bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BtreeSize {
    pub(crate) pages: f64,
    pub(crate) height: u32,
    pub(crate) leaves: f64,
}

/// `nulls` of `entries` hold a NULL in a single key column: they sort last
/// as their own run, which a deduplicating build merges like equal keys, in
/// tuples of a header and a null bitmap; `distinct` counts the other keys.
pub(crate) fn btree_size(
    entries: f64,
    key_width: f64,
    distinct: f64,
    deduplicate: bool,
    nulls: f64,
) -> BtreeSize {
    let key = (key_width.ceil() as usize).next_multiple_of(8);
    let tuple = 8 + key;
    // The leaf tuples in key order: the keys', then the NULL keys' last
    // (one header and null bitmap each). A posting tuple holds a run of
    // equal keys, whose posting list (padding included) a page's fill test
    // discounts as truncatable (`truncextra`).
    let entries = entries.max(0.0);
    let nulls = nulls.clamp(0.0, entries).round_ties_even();
    let keyed = entries - nulls;
    let postings = |count: f64, keys: f64, tuple: usize| -> Run {
        if deduplicate && keys >= 1.0 && count / keys >= 2.0 {
            let per_key = count / keys;
            let per_posting = ((812 - tuple) / 6).max(1) as f64;
            let postings = (per_key / per_posting).ceil();
            let posting =
                ((tuple as f64 + 6.0 * per_key / postings).ceil() as usize).next_multiple_of(8);
            Run {
                tuples: keys * postings,
                size: posting,
                truncatable: posting - tuple,
            }
        } else {
            Run {
                tuples: count,
                size: tuple,
                truncatable: 0,
            }
        }
    };
    let leaf_runs = [
        postings(keyed, distinct, tuple),
        postings(nulls, if nulls >= 1.0 { 1.0 } else { 0.0 }, 16),
    ];
    let leaves = level_pages(&leaf_runs, 819, true);
    // Inner levels hold one pivot per page below, filled to 70%.
    let mut level = leaves;
    let mut pages = 1.0 + leaves;
    let mut height = 0;
    while level > 1.0 {
        level = level_pages(
            &[Run {
                tuples: level,
                size: tuple,
                truncatable: 0,
            }],
            2457,
            false,
        );
        pages += level;
        height += 1;
    }
    BtreeSize {
        pages,
        height,
        leaves,
    }
}

/// Consecutive index tuples of one size.
#[derive(Clone, Copy)]
struct Run {
    tuples: f64,
    size: usize,
    truncatable: usize,
}

/// The pages `_bt_buildadd` fills with `runs` of tuples in order: a page
/// closes when a tuple arrives and the page's free space
/// (`PageGetFreeSpace`), with its last tuple's truncatable bytes, is under
/// `reserve` while it holds two tuples (or a leaf has no room for the tuple
/// and a heap TID), and its last tuple moves to the next page.
fn level_pages(runs: &[Run], reserve: usize, leaf: bool) -> f64 {
    // `BLCKSZ` less the page header, the special space and the high key's
    // line pointer `_bt_blnewpage` reserves, less one line pointer.
    const FREE: usize = 8192 - 24 - 16 - 4 - 4;
    let mut pages = 1.0f64;
    // The tuples on the open page, their line pointers' bytes included,
    // and its last tuple.
    let mut held = 0usize;
    let mut used = 0usize;
    let mut last: Option<(usize, usize)> = None;
    let closes = |used: usize, held: usize, last: Option<(usize, usize)>, size: usize| {
        let free = FREE.saturating_sub(used);
        let room = size + if leaf { 8 } else { 0 };
        free < room
            || (held >= 2 && free + last.map_or(0, |(_, truncatable)| truncatable) < reserve)
    };
    for run in runs {
        let line = run.size + 4;
        let mut remaining = run.tuples.max(0.0);
        while remaining > 0.0 {
            if held == 1 && last == Some((run.size, run.truncatable)) {
                // A page opened by this run's moved tuple: each later page
                // takes the same tuples, so whole pages are counted.
                let mut full = 1usize;
                while !closes(full * line, full, last, run.size) {
                    full += 1;
                }
                let per_page = (full - 1) as f64;
                let closed = ((remaining - per_page) / per_page).ceil().max(0.0);
                pages += closed;
                remaining -= closed * per_page;
                held = 1 + remaining.ceil() as usize;
                used = held * line;
                break;
            }
            if held > 1 && closes(used, held, last, run.size) {
                pages += 1.0;
                let (moved, _) = last.expect("a closed page holds tuples");
                held = 1;
                used = moved + 4;
                continue;
            }
            held += 1;
            used += line;
            last = Some((run.size, run.truncatable));
            remaining -= 1.0;
        }
    }
    pages
}

/// Whether a btree on `ctype` deduplicates equal keys (its operator class
/// is `equalimage`).
fn deduplicates(ctype: ColType, collation: Collation) -> bool {
    match ctype {
        ColType::Int2
        | ColType::Int4
        | ColType::Int8
        | ColType::Oid
        | ColType::Bool
        | ColType::Char
        | ColType::Date
        | ColType::Time
        | ColType::Timestamp
        | ColType::Timestamptz
        | ColType::Bytea
        | ColType::Uuid => true,
        ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name => {
            c_collation(collation)
        }
        _ => false,
    }
}

/// The pages, height, leaf pages and deduplication of the btree `index`
/// over `entries` rows, as CREATE INDEX lays it out: its tuples hold every key and
/// included column, each aligned, and equal keys share posting lists when
/// every key column's class allows it and nothing is included.
fn btree_layout(
    planner: &Planner<'_, '_>,
    info: &IndexInfo,
    entries: f64,
) -> (f64, u32, f64, bool) {
    let index = &info.catalog;
    let definition = planner.definition;
    let mut width = 0.0f64;
    let mut deduplicate = !index.unique && index.n_include_cols == 0;
    for (position, &column) in index.columns[..index.n_cols]
        .iter()
        .chain(&index.include_columns[..index.n_include_cols])
        .enumerate()
    {
        let column = usize::from(column);
        let ctype = planner.column_type(column);
        let layout = ctype.physical_layout();
        if let Some(variable) = planner.variables.get(column) {
            if layout.length > 0 {
                let align = match layout.alignment {
                    b'c' => 1.0,
                    b's' => 2.0,
                    b'i' => 4.0,
                    _ => 8.0,
                };
                width = (width / align).ceil() * align + f64::from(layout.length);
            } else {
                // An index stores a short value with a one-byte header where
                // ANALYZE measured the expression's four-byte one.
                width += variable
                    .statistics
                    .and_then(|(index, key)| planner.statistics.index_column(index, key))
                    .map_or(32.0, |column| f64::from(column.average_width) - 3.0);
            }
            if position < index.n_cols {
                deduplicate &= deduplicates(ctype, variable.collation);
            }
            continue;
        }
        let meta = &definition.columns()[column];
        if layout.length > 0 {
            let align = match layout.alignment {
                b'c' => 1.0,
                b's' => 2.0,
                b'i' => 4.0,
                _ => 8.0,
            };
            width = (width / align).ceil() * align + f64::from(layout.length);
        } else {
            width += definition
                .attributes
                .number(column)
                .and_then(|attribute| planner.statistics.column(attribute))
                .map_or(32.0, |column| f64::from(column.average_width));
        }
        if position < index.n_cols {
            deduplicate &=
                !index.expression_keys[position] && deduplicates(meta.ctype, meta.collation);
        }
    }
    // Distinct keys: a unique index's every entry; else the key columns'
    // distinct combinations as `estimate_num_groups` counts them over the
    // entries: the product of each column's distinct values, clamped to a
    // tenth of the entries for several columns (but no fewer than the most
    // any one column has), and to the entries.
    let distinct = if index.unique {
        entries
    } else {
        let mut product = 1.0;
        let mut largest = 0.0f64;
        let mut counted = 0;
        let mut doubled = 1.0;
        for (position, &column) in index.columns[..index.n_cols].iter().enumerate() {
            // A boolean key has two values.
            if !index.expression_keys[position]
                && planner.column_type(usize::from(column)) == ColType::Bool
            {
                doubled *= 2.0;
                continue;
            }
            // An expression key's own statistics count its distinct values,
            // a partial index's too.
            let own = (index.expression_keys[position])
                .then(|| {
                    planner
                        .statistics
                        .index_column(info.stamp, position as u16 + 1)
                })
                .flatten();
            let distinct = match own {
                Some(statistics) if statistics.distinct_fraction != 0.0 => {
                    clamp_row_est(f64::from(statistics.distinct_fraction) * entries)
                }
                Some(statistics) if statistics.distinct_values > 0 => {
                    statistics.distinct_values as f64
                }
                _ => planner.distinct(&planner.variable(usize::from(column))).0,
            };
            product *= distinct;
            largest = largest.max(distinct);
            counted += 1;
        }
        if counted > 0 && entries > 0.0 {
            let mut clamp = entries;
            if counted > 1 {
                clamp *= 0.1;
                if clamp < largest {
                    clamp = largest.min(entries);
                }
            }
            product = clamp_row_est(product.min(clamp));
        }
        (product * doubled).ceil().max(1.0)
    };
    let distinct = distinct.min(entries.max(1.0));
    // A single key column's NULL keys sort last in their own run.
    let nulls = if index.n_cols == 1 && !index.expression_keys[0] {
        entries
            * planner
                .variable(usize::from(index.columns[0]))
                .null_fraction
    } else {
        0.0
    };
    let size = btree_size(entries, width, distinct, deduplicate, nulls);
    (size.pages, size.height, size.leaves, deduplicate)
}

/// Whether `collation` orders strings byte by byte: the database default
/// (`C`) and the built-in collations do.
fn c_collation(collation: Collation) -> bool {
    !matches!(collation, Collation::Catalog(_))
}

/// A comparison operator with the column on its left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Comparison {
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
}

impl Comparison {
    fn of(operator: BinaryOp, column_on_left: bool) -> Option<Self> {
        let operator = match operator {
            BinaryOp::Lt => Self::Lt,
            BinaryOp::LtEq => Self::Le,
            BinaryOp::Eq => Self::Eq,
            BinaryOp::NotEq => Self::Ne,
            BinaryOp::GtEq => Self::Ge,
            BinaryOp::Gt => Self::Gt,
            _ => return None,
        };
        Some(if column_on_left {
            operator
        } else {
            operator.commuted()
        })
    }

    fn commuted(self) -> Self {
        match self {
            Self::Lt => Self::Gt,
            Self::Le => Self::Ge,
            Self::Ge => Self::Le,
            Self::Gt => Self::Lt,
            other => other,
        }
    }

    fn binary(self) -> BinaryOp {
        match self {
            Self::Lt => BinaryOp::Lt,
            Self::Le => BinaryOp::LtEq,
            Self::Eq => BinaryOp::Eq,
            Self::Ne => BinaryOp::NotEq,
            Self::Ge => BinaryOp::GtEq,
            Self::Gt => BinaryOp::Gt,
        }
    }

    /// The operator's result for `left` ordered `order` against `right`.
    fn holds(self, order: Ordering) -> bool {
        match self {
            Self::Lt => order.is_lt(),
            Self::Le => order.is_le(),
            Self::Eq => order.is_eq(),
            Self::Ne => order.is_ne(),
            Self::Ge => order.is_ge(),
            Self::Gt => order.is_gt(),
        }
    }
}

/// The kind of scan PostgreSQL chose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ScanKind {
    Seq,
    Index,
    IndexOnly,
    Bitmap,
}

/// An index condition: the restriction it comes from, and the condition
/// as the index applies it, its indexed side on the left (an `OR` of
/// constants as `= ANY`).
#[derive(Clone, Copy)]
pub(crate) struct IndexCondition<'a> {
    pub(crate) source: &'a Expr<'a>,
    pub(crate) printed: &'a Expr<'a>,
}

/// One node of a bitmap heap scan's bitmap, in plan order: a bitmap index
/// scan, or the `BitmapAnd` or `BitmapOr` of the nodes after it one level
/// deeper.
#[derive(Clone, Copy)]
pub(crate) struct BitmapPlanNode<'a> {
    pub(crate) kind: BitmapPlanKind<'a>,
    /// Levels below the bitmap heap scan, from 1.
    pub(crate) depth: u8,
    pub(crate) total_cost: f64,
    pub(crate) rows: f64,
}

#[derive(Clone, Copy)]
pub(crate) enum BitmapPlanKind<'a> {
    Index {
        index_name: crate::util::StackStr<64>,
        /// The pages the scan reads of its index.
        pages: IndexPages,
        conditions: &'a [IndexCondition<'a>],
        /// A partial index's predicate, which every row it holds meets.
        predicate: &'a [&'a Expr<'a>],
    },
    And,
    Or,
}

/// The sort above the chosen scan.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SortKind {
    /// The scan returns the requested order, or none was requested.
    None,
    Full,
    /// An incremental sort over rows the scan orders by this many leading
    /// keys.
    Incremental(usize),
}

/// One key column of the chosen index: the column (or the number of its
/// key expression), and whether the index stores it descending and its
/// NULLs first.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IndexKeyOrder {
    pub(crate) column: usize,
    pub(crate) descending: bool,
    pub(crate) nulls_first: bool,
    /// An expression key, which [`KeyReader`] evaluates.
    pub(crate) expression: Option<KeyExpression>,
}

/// An index key that is an expression: its key position and the collation
/// it is ordered under.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyExpression {
    pub(crate) position: u8,
    pub(crate) collation: Collation,
}

impl IndexKeyOrder {
    /// The collation the key orders under on a table defined by
    /// `definition`.
    pub(crate) fn collation(&self, definition: &TableDef) -> Collation {
        self.expression.map_or_else(
            || definition.columns()[self.column].collation,
            |expression| expression.collation,
        )
    }
}

/// The path PostgreSQL chose for the relation.
#[derive(Clone, Copy)]
pub(crate) struct ScanPath<'a> {
    pub(crate) kind: ScanKind,
    pub(crate) index_oid: i32,
    pub(crate) index_name: crate::util::StackStr<64>,
    /// The pages an index or index-only scan reads of its index.
    pub(crate) index_pages: IndexPages,
    /// The index's key columns, for an index or index-only scan.
    pub(crate) keys: &'a [IndexKeyOrder],
    /// An index scan's conditions, in index column order.
    pub(crate) conditions: &'a [IndexCondition<'a>],
    /// A partial index's predicate, which every row it reaches meets.
    pub(crate) predicate: &'a [&'a Expr<'a>],
    /// A bitmap heap scan's bitmap.
    pub(crate) bitmap: &'a [BitmapPlanNode<'a>],
    /// A bitmap heap scan's `Recheck Cond`, its parts AND-ed.
    pub(crate) recheck: &'a [&'a Expr<'a>],
    /// The restrictions the index conditions or the bitmap apply, which no
    /// `Filter` repeats.
    pub(crate) absorbed: &'a [&'a Expr<'a>],
    /// The estimated rows the scan returns and their width.
    pub(crate) rows: f64,
    pub(crate) width: u32,
    pub(crate) startup_cost: f64,
    pub(crate) total_cost: f64,
    /// The index scan reads its index backward.
    pub(crate) backward: bool,
    pub(crate) sort: SortKind,
    /// Equal keys share posting lists, which a scan in either direction
    /// reads in row identity order.
    pub(crate) deduplicated: bool,
    /// The rows a `LIMIT` above an ordered scan takes before it stops.
    pub(crate) stop_after: Option<u64>,
    /// A unique index's equalities reach at most one row, whose order
    /// nothing else decides.
    pub(crate) single_row: bool,
    /// An index scan searches an array or bridges a key column with a skip
    /// array over an index of several leaf pages, so its descents depend on
    /// the leaf pages its keys fall on.
    pub(crate) searches_unmodeled: bool,
    /// The startup and total cost of the sort above the scan.
    pub(crate) sort_cost: Option<(f64, f64)>,
    /// The startup and total cost and the rows of the `Limit` above it.
    pub(crate) limit_cost: Option<(f64, f64, f64)>,
    /// The grouping above the scan, for a [`QueryShape::grouping`].
    pub(crate) grouped: Option<GroupedPath>,
}

/// What the model makes of a query's scan of one relation.
#[derive(Clone, Copy)]
#[allow(
    clippy::large_enum_variant,
    reason = "the chosen path is returned inline; boxing would heap-allocate"
)]
pub(crate) enum Planned<'a> {
    /// The path PostgreSQL chooses.
    Chosen(ScanPath<'a>),
    /// The relation is no plain table: a subquery, a function or a foreign
    /// table, whose scans have their own plans.
    NotTable,
    /// PostgreSQL can only scan the table sequentially, but a condition's
    /// selectivity is outside the model: the plan is exact, its estimates
    /// are not PostgreSQL's.
    SeqOnly,
    /// PostgreSQL plans the scan in a way the model does not cover; the
    /// text names the shape.
    Outside(&'static str),
    /// The paths of a member of a tree ([`Member::Paths`]).
    Members(paths::RelPaths<'a>),
    /// The `Append` or `Merge Append` PostgreSQL chooses over the members
    /// of a partition or inheritance tree.
    Tree(append::TreePath<'a>),
    /// The paths of a base relation of a join ([`Member::Join`]).
    Join(paths::JoinRelPaths<'a>),
}

impl<'a> Planned<'a> {
    pub(crate) fn chosen(self) -> Option<ScanPath<'a>> {
        match self {
            Self::Chosen(path) => Some(path),
            Self::NotTable
            | Self::SeqOnly
            | Self::Outside(_)
            | Self::Members(_)
            | Self::Tree(_)
            | Self::Join(_) => None,
        }
    }

    /// The path chosen over a partition or inheritance tree.
    pub(crate) fn tree(self) -> Option<append::TreePath<'a>> {
        match self {
            Self::Tree(path) => Some(path),
            _ => None,
        }
    }
}

/// A column's statistics as `examine_variable` finds them.
pub(super) struct VariableStatistics<'s> {
    /// Whether ANALYZE recorded the column (`statsTuple`).
    pub(super) analyzed: bool,
    pub(super) null_fraction: f64,
    distinct: f64,
    pub(super) distribution: Option<ColumnDistribution<'s>>,
    ctype: ColType,
    collation: Collation,
    unique: bool,
    /// A btree index leads with the column (`get_actual_variable_range`).
    indexed: bool,
}

/// An index as `get_relation_info` describes it (`IndexOptInfo`).
#[derive(Clone, Copy)]
pub(super) struct IndexInfo {
    pub(super) catalog: PlannerIndex,
    /// The index's creation stamp, which keys its expressions' statistics.
    pub(super) stamp: u64,
    /// The index's pages and tree height as CREATE INDEX lays them out.
    pub(super) pages: f64,
    pub(super) height: u32,
    /// Its leaf pages, which hold its entries.
    pub(super) leaves: f64,
    /// Equal keys share posting lists.
    pub(super) deduplicated: bool,
    /// Its entries (`IndexOptInfo.tuples`): the table's rows, or a partial
    /// index's rows that satisfy its predicate.
    pub(super) tuples: f64,
}

/// The pages a btree search reads, as its modeled layout places them: the
/// pages of one descent from the root to a leaf, then the further leaf
/// pages its matching entries fill.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct IndexPages {
    /// The levels from the root to the leaves, the leaves included.
    pub(crate) levels: u32,
    /// The entries one leaf page holds on average.
    pub(crate) leaf_entries: f64,
}

impl IndexPages {
    /// No index: a sequential or bitmap heap scan's.
    pub(crate) const NONE: Self = Self {
        levels: 0,
        leaf_entries: 1.0,
    };

    pub(super) fn of(info: &IndexInfo) -> Self {
        Self {
            levels: info.height + 1,
            leaf_entries: (info.tuples / info.leaves.max(1.0)).max(1.0),
        }
    }

    /// The index pages one search reaching `entries` entries reads: its
    /// descent, and each further leaf page its entries continue on.
    pub(crate) fn search(self, entries: u64) -> u64 {
        let leaves = (entries as f64 / self.leaf_entries).ceil() as u64;
        u64::from(self.levels) + leaves.saturating_sub(1)
    }
}

impl IndexInfo {
    pub(super) fn key_columns(&self) -> impl Iterator<Item = usize> + '_ {
        self.catalog.columns[..self.catalog.n_cols]
            .iter()
            .map(|&column| usize::from(column))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Planner<'s, 'a> {
    /// The key expressions of the table's indexes, numbered after its
    /// columns.
    pub(super) variables: ExpressionVariables<'a>,
    pub(super) storage: &'s Storage,
    pub(super) txid: u32,
    pub(super) slot: usize,
    pub(super) definition: &'s TableDef,
    pub(super) statistics: &'s TableStatistics,
    /// The key of the statistics its columns read: the table's own (0), or
    /// those over a tree ([`crate::storage::INHERITED`]).
    pub(super) statistics_key: u64,
    pub(super) size: RelationSize,
    pub(super) costs: Costs,
    /// Every index of the table, newest first.
    pub(super) indexes: &'s [IndexInfo],
    /// The table's statistics objects.
    pub(super) extended: &'a [extended::ExtendedObject<'a>],
    pub(super) arena: &'a Arena,
}

impl<'s, 'a: 's> Planner<'s, 'a> {
    /// The type of column or key expression `column`.
    pub(super) fn column_type(&self, column: usize) -> ColType {
        match self.variables.get(column) {
            Some(variable) => variable.ctype,
            None => self.definition.columns()[column].ctype,
        }
    }

    /// The collation column or key expression `column` compares under.
    pub(super) fn column_collation(&self, column: usize) -> Collation {
        match self.variables.get(column) {
            Some(variable) => variable.collation,
            None => self.definition.columns()[column].collation,
        }
    }

    pub(super) fn variable(&self, column: usize) -> VariableStatistics<'s> {
        if let Some(variable) = self.variables.get(column) {
            let analyzed = variable
                .statistics
                .and_then(|(index, key)| self.statistics.index_column(index, key))
                .or_else(|| variable.extended.map(|extended| extended.column));
            return VariableStatistics {
                analyzed: analyzed.is_some(),
                null_fraction: analyzed.map_or(0.0, |column| f64::from(column.null_fraction)),
                distinct: analyzed.map_or(0.0, |column| {
                    if column.distinct_fraction != 0.0 {
                        -f64::from(column.distinct_fraction)
                    } else {
                        column.distinct_values as f64
                    }
                }),
                distribution: variable
                    .statistics
                    .and_then(|(index, key)| self.statistics.index_distribution(index, key))
                    .or_else(|| {
                        variable
                            .extended
                            .and_then(|extended| extended.distribution.distribution())
                    }),
                ctype: variable.ctype,
                collation: variable.collation,
                unique: variable.unique,
                indexed: variable.indexed,
            };
        }
        let meta = &self.definition.columns()[column];
        let attribute = self.definition.attributes.number(column);
        let analyzed = attribute
            .and_then(|attribute| self.statistics.index_column(self.statistics_key, attribute));
        // `has_unique_index`.
        let unique = self.indexes.iter().any(|index| {
            let index = &index.catalog;
            index.unique
                && index.n_cols == 1
                && !index.partial
                && !index.expression_keys[0]
                && usize::from(index.columns[0]) == column
        });
        // `get_actual_variable_range` reads a whole-table btree whose first
        // key is the column in its own collation.
        let indexed = self.indexes.iter().any(|index| {
            let index = &index.catalog;
            index.method == IndexAccessMethod::Btree
                && index.n_cols >= 1
                && !index.partial
                && !index.expression_keys[0]
                && index.column_collation[0]
                && usize::from(index.columns[0]) == column
        });
        VariableStatistics {
            analyzed: analyzed.is_some(),
            null_fraction: analyzed.map_or(0.0, |column| f64::from(column.null_fraction)),
            distinct: analyzed.map_or(0.0, |column| {
                f64::from(crate::storage::column_distinct_catalog_value(meta, column))
            }),
            distribution: attribute.and_then(|attribute| {
                self.statistics
                    .index_distribution(self.statistics_key, attribute)
            }),
            ctype: meta.ctype,
            collation: meta.collation,
            unique,
            indexed,
        }
    }

    /// `get_variable_numdistinct`.
    pub(super) fn distinct(&self, variable: &VariableStatistics<'_>) -> (f64, bool) {
        let mut stadistinct = if variable.analyzed {
            variable.distinct
        } else if variable.ctype == ColType::Bool {
            2.0
        } else {
            0.0
        };
        if variable.unique {
            stadistinct = -(1.0 - variable.null_fraction);
        }
        if stadistinct > 0.0 {
            return (clamp_row_est(stadistinct), false);
        }
        let tuples = self.size.tuples;
        if tuples <= 0.0 {
            return (DEFAULT_NUM_DISTINCT, true);
        }
        if stadistinct < 0.0 {
            return (clamp_row_est(-stadistinct * tuples), false);
        }
        // A small relation's rows, which is no default.
        if tuples < DEFAULT_NUM_DISTINCT {
            return (clamp_row_est(tuples), false);
        }
        (DEFAULT_NUM_DISTINCT, true)
    }

    pub(super) fn compare(
        &self,
        collation: Collation,
        left: &Datum<'_>,
        right: &Datum<'_>,
    ) -> Result<Ordering, SqlError> {
        crate::sql::eval::compare_datums_collated(self.storage, self.txid, collation, left, right)
    }

    /// `var_eq_const`, negated for `<>`.
    fn equality_selectivity(
        &self,
        column: usize,
        constant: Datum<'_>,
        negate: bool,
    ) -> Result<f64, SqlError> {
        if constant.is_null() {
            return Ok(0.0);
        }
        let variable = self.variable(column);
        let null_fraction = variable.null_fraction;
        let mut selectivity = if variable.unique && self.size.tuples >= 1.0 {
            1.0 / self.size.tuples
        } else if variable.analyzed {
            let distribution = variable.distribution;
            let mcv = distribution.map_or(0, |distribution| distribution.mcv_len());
            let mut matched = None;
            if let Some(distribution) = distribution {
                for index in 0..mcv {
                    let value = distribution.mcv_value(index, variable.ctype)?;
                    if self.compare(variable.collation, &value, &constant)?.is_eq() {
                        matched = Some(index);
                        break;
                    }
                }
            }
            match (matched, distribution) {
                (Some(index), Some(distribution)) => f64::from(distribution.mcv_frequency(index)),
                _ => {
                    let common: f64 = distribution.map_or(0.0, |distribution| {
                        (0..mcv)
                            .map(|index| f64::from(distribution.mcv_frequency(index)))
                            .sum()
                    });
                    let mut selectivity = clamp_probability(1.0 - common - null_fraction);
                    let other = self.distinct(&variable).0 - mcv as f64;
                    if other > 1.0 {
                        selectivity /= other;
                    }
                    if let Some(distribution) = distribution.filter(|_| mcv > 0) {
                        let least = f64::from(distribution.mcv_frequency(mcv - 1));
                        if selectivity > least {
                            selectivity = least;
                        }
                    }
                    selectivity
                }
            }
        } else {
            1.0 / self.distinct(&variable).0
        };
        if negate {
            selectivity = 1.0 - selectivity - null_fraction;
        }
        Ok(clamp_probability(selectivity))
    }

    /// `var_eq_non_const`: `column = value` for a value unknown at planning,
    /// negated for `<>`.
    fn unknown_equality_selectivity(&self, column: usize, negate: bool) -> f64 {
        let variable = self.variable(column);
        let null_fraction = variable.null_fraction;
        let mut selectivity = if variable.unique && self.size.tuples >= 1.0 {
            1.0 / self.size.tuples
        } else if variable.analyzed {
            let mut selectivity = 1.0 - null_fraction;
            let distinct = self.distinct(&variable).0;
            if distinct > 1.0 {
                selectivity /= distinct;
            }
            if let Some(distribution) = variable.distribution.filter(|d| d.mcv_len() > 0) {
                let most = f64::from(distribution.mcv_frequency(0));
                if selectivity > most {
                    selectivity = most;
                }
            }
            selectivity
        } else {
            1.0 / self.distinct(&variable).0
        };
        if negate {
            selectivity = 1.0 - selectivity - null_fraction;
        }
        clamp_probability(selectivity)
    }

    /// `scalarineqsel` for `column comparison constant`.
    fn inequality_selectivity(
        &self,
        column: usize,
        comparison: Comparison,
        constant: Datum<'_>,
    ) -> Result<Result<f64, ()>, SqlError> {
        if constant.is_null() {
            return Ok(Ok(0.0));
        }
        let variable = self.variable(column);
        if !variable.analyzed {
            return Ok(Ok(DEFAULT_INEQ_SEL));
        }
        let mut mcv_selectivity = 0.0;
        let mut common = 0.0;
        if let Some(distribution) = variable.distribution {
            for index in 0..distribution.mcv_len() {
                let value = distribution.mcv_value(index, variable.ctype)?;
                let frequency = f64::from(distribution.mcv_frequency(index));
                if comparison.holds(self.compare(variable.collation, &value, &constant)?) {
                    mcv_selectivity += frequency;
                }
                common += frequency;
            }
        }
        let histogram = match self.histogram_selectivity(&variable, column, comparison, constant)? {
            Ok(histogram) => histogram,
            Err(()) => return Ok(Err(())),
        };
        let mut selectivity = 1.0 - variable.null_fraction - common;
        selectivity *= histogram.unwrap_or(0.5);
        selectivity += mcv_selectivity;
        Ok(Ok(clamp_probability(selectivity)))
    }

    /// `ineq_histogram_selectivity`: `None` without a histogram; `Err`
    /// when PostgreSQL's interpolation is outside the model.
    pub(super) fn histogram_selectivity(
        &self,
        variable: &VariableStatistics<'_>,
        column: usize,
        comparison: Comparison,
        constant: Datum<'_>,
    ) -> Result<Result<Option<f64>, ()>, SqlError> {
        let Some(distribution) = variable.distribution.filter(|d| d.histogram_len() > 1) else {
            return Ok(Ok(None));
        };
        let count = distribution.histogram_len();
        let greater = matches!(comparison, Comparison::Gt | Comparison::Ge);
        let inclusive = matches!(comparison, Comparison::Le | Comparison::Ge);
        let mut bounds_low = distribution.histogram_value(0, variable.ctype)?;
        let mut bounds_high = distribution.histogram_value(count - 1, variable.ctype)?;
        let mut have_end = false;
        // `get_actual_variable_range` over the index's current ends, read
        // once when a probe reaches an end bound.
        let mut actual: Option<Option<(Datum<'_>, Datum<'_>)>> = None;
        let mut fetch_actual =
            |planner: &Self| -> Result<Result<Option<(Datum<'_>, Datum<'_>)>, ()>, SqlError> {
                if !variable.indexed {
                    return Ok(Ok(None));
                }
                if actual.is_none() {
                    match planner.actual_range(variable, column)? {
                        Ok(range) => actual = Some(range),
                        Err(()) => return Ok(Err(())),
                    }
                }
                Ok(Ok(actual.flatten()))
            };
        if count == 2 {
            match fetch_actual(self)? {
                Ok(Some((low, high))) => {
                    bounds_low = low;
                    bounds_high = high;
                    have_end = true;
                }
                Ok(None) => {}
                Err(()) => return Ok(Err(())),
            }
        }
        fn value_at<'v>(
            distribution: &ColumnDistribution<'v>,
            ctype: ColType,
            index: usize,
            low: Datum<'v>,
            high: Datum<'v>,
        ) -> Result<Datum<'v>, SqlError> {
            if index == 0 {
                Ok(low)
            } else if index == distribution.histogram_len() - 1 {
                Ok(high)
            } else {
                distribution.histogram_value(index, ctype)
            }
        }
        let mut lower = 0usize;
        let mut upper = count;
        while lower < upper {
            let probe = (lower + upper) / 2;
            if (probe == 0 || probe == count - 1) && count > 2 {
                match fetch_actual(self)? {
                    Ok(Some((low, high))) => {
                        if probe == 0 {
                            bounds_low = low;
                        } else {
                            bounds_high = high;
                        }
                        have_end = true;
                    }
                    Ok(None) => have_end = false,
                    Err(()) => return Ok(Err(())),
                }
            }
            let bound = value_at(
                &distribution,
                variable.ctype,
                probe,
                bounds_low,
                bounds_high,
            )?;
            let mut less = comparison.holds(self.compare(variable.collation, &bound, &constant)?);
            if greater {
                less = !less;
            }
            if less {
                lower = probe + 1;
            } else {
                upper = probe;
            }
        }
        let fraction = if lower == 0 {
            0.0
        } else if lower >= count {
            1.0
        } else {
            let index = lower;
            let mut equal = 0.0;
            if index == 1 || greater == inclusive {
                let mut other = self.distinct(variable).0;
                other -= distribution.mcv_len() as f64;
                if other > 1.0 {
                    equal = 1.0 / other;
                }
            }
            let low = value_at(
                &distribution,
                variable.ctype,
                index - 1,
                bounds_low,
                bounds_high,
            )?;
            let high = value_at(
                &distribution,
                variable.ctype,
                index,
                bounds_low,
                bounds_high,
            )?;
            let binary_fraction = match scalar_positions(&constant, &low, &high, variable.collation)
            {
                Scalar::Values(value, low, high) => {
                    if high <= low {
                        0.5
                    } else if value <= low {
                        0.0
                    } else if value >= high {
                        1.0
                    } else {
                        let fraction = (value - low) / (high - low);
                        if fraction.is_nan() || !(0.0..=1.0).contains(&fraction) {
                            0.5
                        } else {
                            fraction
                        }
                    }
                }
                Scalar::Unconvertible => 0.5,
                Scalar::OutsideModel => return Ok(Err(())),
            };
            let mut fraction = ((index - 1) as f64 + binary_fraction) / (count - 1) as f64;
            if index == 1 {
                fraction += equal * (1.0 - binary_fraction);
            }
            if greater == inclusive {
                fraction -= equal;
            }
            fraction
        };
        let mut selectivity = if greater { 1.0 - fraction } else { fraction };
        if have_end {
            selectivity = clamp_probability(selectivity);
        } else {
            let cutoff = 0.01 / (count - 1) as f64;
            selectivity = selectivity.clamp(cutoff, 1.0 - cutoff);
        }
        Ok(Ok(Some(selectivity)))
    }

    /// `get_actual_variable_range`: the column's smallest and largest
    /// current non-NULL values, which PostgreSQL reads from the index; `Err`
    /// when a value is too wide to hold.
    fn actual_range(
        &self,
        variable: &VariableStatistics<'_>,
        column: usize,
    ) -> Result<Result<Option<(Datum<'a>, Datum<'a>)>, ()>, SqlError> {
        const HELD: usize = 2048;
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = self.definition.schema(&mut schema);
        let schema = &schema[..n_columns];
        let mut low = [0u8; HELD];
        let mut high = [0u8; HELD];
        let (mut low_length, mut high_length) = (0usize, 0usize);
        let mut too_wide = false;
        let mut failure = None;
        let mut observe = |value: Result<Datum<'_>, SqlError>| {
            let value = match value {
                Ok(value) => value,
                Err(error) => {
                    failure.get_or_insert(error);
                    return;
                }
            };
            if value.is_null() || too_wide || failure.is_some() {
                return;
            }
            let length = crate::storage::rowenc::encoded_len(core::slice::from_ref(&value));
            if length > HELD {
                too_wide = true;
                return;
            }
            for (held, held_length, wanted) in [
                (&mut low, &mut low_length, Ordering::Less),
                (&mut high, &mut high_length, Ordering::Greater),
            ] {
                let replace = if *held_length == 0 {
                    true
                } else {
                    let mut current = [Datum::Null];
                    if let Err(error) = crate::storage::rowenc::decode(
                        &held[..*held_length],
                        &[variable.ctype],
                        &mut current,
                    ) {
                        failure = Some(error);
                        return;
                    }
                    match self.compare(variable.collation, &value, &current[0]) {
                        Ok(order) => order == wanted,
                        Err(error) => {
                            failure = Some(error);
                            return;
                        }
                    }
                };
                if replace {
                    crate::storage::rowenc::encode(
                        core::slice::from_ref(&value),
                        &mut held[..length],
                    );
                    *held_length = length;
                }
            }
        };
        let scan = self
            .storage
            .captured_row_scan_in(self.slot, self.txid, self.arena)?;
        // A sequential scan's order: saved rows, then resident rows.
        scan.for_each_spilled_row_batch(self.arena, true, None, &mut |batch| {
            for row in batch {
                let mut values = [Datum::Null; MAX_COLUMNS];
                let mark = self.arena.mark();
                let value = match row.representation {
                    crate::storage::SpilledRowRepresentation::Encoded(bytes) => {
                        crate::storage::rowenc::decode(bytes, schema, &mut values)?;
                        self.row_value(column, &values[..n_columns])
                    }
                    crate::storage::SpilledRowRepresentation::Values(values) => {
                        self.row_value(column, values)
                    }
                };
                observe(value);
                // SAFETY: the evaluated key was copied out above.
                unsafe { self.arena.rewind_to(mark) };
            }
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        let _ = scan.for_each_resident_physically(self.arena, &mut |rowid, home| {
            self.storage
                .with_row_bytes(self.slot, rowid, home, |bytes| {
                    let mut values = [Datum::Null; MAX_COLUMNS];
                    crate::storage::rowenc::decode(bytes, schema, &mut values)?;
                    let mark = self.arena.mark();
                    observe(self.row_value(column, &values[..n_columns]));
                    // SAFETY: the evaluated key was copied out above.
                    unsafe { self.arena.rewind_to(mark) };
                    Ok(())
                })?;
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        drop(scan);
        if let Some(error) = failure {
            return Err(error);
        }
        if too_wide {
            return Ok(Err(()));
        }
        if low_length == 0 {
            return Ok(Ok(None));
        }
        let decode = |held: &[u8]| -> Result<Datum<'a>, SqlError> {
            let bytes = self
                .arena
                .alloc_slice_copy(held)
                .map_err(|_| crate::sql::eval::arena_full())?;
            let mut out = [Datum::Null];
            crate::storage::rowenc::decode(bytes, &[variable.ctype], &mut out)?;
            Ok(out[0])
        };
        Ok(Ok(Some((
            decode(&low[..low_length])?,
            decode(&high[..high_length])?,
        ))))
    }

    /// The value of column or key expression `column` in the row `values`.
    fn row_value<'v>(&self, column: usize, values: &[Datum<'v>]) -> Result<Datum<'v>, SqlError>
    where
        'a: 'v,
    {
        expressions::row_value(
            self.storage,
            self.txid,
            self.definition,
            &self.variables,
            column,
            values,
            self.arena,
        )
    }

    /// The visible rows of the table that satisfy `predicate`, as a partial
    /// index built over them counts its entries; `None` when the predicate
    /// has a condition the model cannot evaluate.
    pub(super) fn rows_matching(
        &self,
        predicate: &predicate::Prop<'_>,
    ) -> Result<Option<f64>, SqlError> {
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = self.definition.schema(&mut schema);
        let schema = &schema[..n_columns];
        let mut count = 0u64;
        let mut failure: Option<Result<(), SqlError>> = None;
        let mut check = |values: &[Datum<'_>]| {
            if failure.is_some() {
                return;
            }
            match self.holds(predicate, values) {
                Ok(Some(true)) => count += 1,
                Ok(Some(false)) => {}
                Ok(None) => failure = Some(Ok(())),
                Err(error) => failure = Some(Err(error)),
            }
        };
        let scan = self
            .storage
            .captured_row_scan_in(self.slot, self.txid, self.arena)?;
        scan.for_each_spilled_row_batch(self.arena, true, None, &mut |batch| {
            for row in batch {
                let mut values = [Datum::Null; MAX_COLUMNS];
                match row.representation {
                    crate::storage::SpilledRowRepresentation::Encoded(bytes) => {
                        crate::storage::rowenc::decode(bytes, schema, &mut values)?;
                        check(&values[..n_columns]);
                    }
                    crate::storage::SpilledRowRepresentation::Values(values) => check(values),
                }
            }
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        let _ = scan.for_each_resident_physically(self.arena, &mut |rowid, home| {
            self.storage
                .with_row_bytes(self.slot, rowid, home, |bytes| {
                    let mut values = [Datum::Null; MAX_COLUMNS];
                    crate::storage::rowenc::decode(bytes, schema, &mut values)?;
                    check(&values[..n_columns]);
                    Ok(())
                })?;
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        drop(scan);
        match failure {
            Some(Err(error)) => Err(error),
            Some(Ok(())) => Ok(None),
            None => Ok(Some(count as f64)),
        }
    }

    /// Whether the row `values` satisfies `prop` (a NULL result does not);
    /// `None` when `prop` holds a condition the model cannot evaluate.
    fn holds(
        &self,
        prop: &predicate::Prop<'_>,
        values: &[Datum<'_>],
    ) -> Result<Option<bool>, SqlError> {
        use predicate::Prop;
        // A key expression's value is not evaluated here.
        if prop_reads_expression(prop, &self.variables) {
            return Ok(None);
        }
        let compare = |column: usize, comparison: Comparison, constant: &Datum<'_>| {
            let value = values[column];
            if value.is_null() || constant.is_null() {
                return Ok(None);
            }
            let collation = self.column_collation(column);
            Ok(Some(
                comparison.holds(self.compare(collation, &value, constant)?),
            ))
        };
        Ok(match *prop {
            Prop::Compare {
                column,
                comparison,
                constant,
                ..
            } => Some(compare(column, comparison, &constant)?.unwrap_or(false)),
            Prop::NullTest { column, is_null } => Some(values[column].is_null() == is_null),
            Prop::Array {
                column,
                comparison,
                any,
                elements,
                ..
            } => {
                let mut result = Some(!any);
                for element in elements {
                    match compare(column, comparison, element)? {
                        Some(true) if any => return Ok(Some(true)),
                        Some(false) if !any => return Ok(Some(false)),
                        None => result = Some(false),
                        _ => {}
                    }
                }
                result
            }
            Prop::And(parts) => {
                for part in parts {
                    match self.holds(part, values)? {
                        Some(true) => {}
                        other => return Ok(other),
                    }
                }
                Some(true)
            }
            Prop::Or(parts) => {
                for part in parts {
                    match self.holds(part, values)? {
                        Some(false) => {}
                        other => return Ok(other),
                    }
                }
                Some(false)
            }
            Prop::Opaque(_) => None,
        })
    }

    /// `nulltestsel`.
    pub(super) fn null_selectivity(&self, column: usize, is_null: bool) -> f64 {
        let variable = self.variable(column);
        if variable.analyzed {
            if is_null {
                variable.null_fraction
            } else {
                1.0 - variable.null_fraction
            }
        } else if is_null {
            DEFAULT_UNK_SEL
        } else {
            DEFAULT_NOT_UNK_SEL
        }
    }

    /// The selectivity of `column comparison constant` (`eqsel`, `neqsel`,
    /// `scalarineqsel`), or the shape outside the model.
    pub(super) fn operator_selectivity(
        &self,
        column: usize,
        comparison: Comparison,
        constant: Option<Datum<'_>>,
    ) -> Result<Result<f64, &'static str>, SqlError> {
        Ok(Ok(match (comparison, constant) {
            (Comparison::Eq, Some(constant)) => {
                self.equality_selectivity(column, constant, false)?
            }
            (Comparison::Ne, Some(constant)) => {
                self.equality_selectivity(column, constant, true)?
            }
            (Comparison::Eq, None) => self.unknown_equality_selectivity(column, false),
            (Comparison::Ne, None) => self.unknown_equality_selectivity(column, true),
            // `scalarineqsel` needs a constant; without one PostgreSQL
            // assumes a third of the rows.
            (_, None) => DEFAULT_INEQ_SEL,
            (_, Some(constant)) => {
                match self.inequality_selectivity(column, comparison, constant)? {
                    Ok(selectivity) => selectivity,
                    Err(()) => {
                        return Ok(Err(
                            "a range over a column of a type PostgreSQL places otherwise",
                        ));
                    }
                }
            }
        }))
    }

    /// `scalarineqsel` of a pattern operator (`~<~` and its kin): its most
    /// common values and its histogram's entries compared byte by byte, the
    /// histogram following another operator family.
    pub(super) fn pattern_selectivity(
        &self,
        column: usize,
        comparison: Comparison,
        constant: Option<Datum<'_>>,
    ) -> Result<Result<f64, &'static str>, SqlError> {
        let Some(constant) = constant else {
            return Ok(Ok(DEFAULT_INEQ_SEL));
        };
        if constant.is_null() {
            return Ok(Ok(0.0));
        }
        let variable = self.variable(column);
        if !variable.analyzed {
            return Ok(Ok(DEFAULT_INEQ_SEL));
        }
        let mut mcv_selectivity = 0.0;
        let mut common = 0.0;
        if let Some(distribution) = variable.distribution {
            for index in 0..distribution.mcv_len() {
                let value = distribution.mcv_value(index, variable.ctype)?;
                let frequency = f64::from(distribution.mcv_frequency(index));
                let (Some(value), Some(constant)) = (string_bytes(&value), string_bytes(&constant))
                else {
                    return Ok(Err("a pattern comparison of a non-string type"));
                };
                if comparison.holds(value.cmp(constant)) {
                    mcv_selectivity += frequency;
                }
                common += frequency;
            }
        }
        // `ineq_histogram_selectivity` of a histogram sorted another way:
        // the share of its entries the comparison accepts, else half.
        let mut histogram = 0.5;
        if let Some(distribution) = variable.distribution.filter(|d| d.histogram_len() > 1) {
            let mut matched = 0usize;
            for index in 0..distribution.histogram_len() {
                let value = distribution.histogram_value(index, variable.ctype)?;
                let (Some(value), Some(constant)) = (string_bytes(&value), string_bytes(&constant))
                else {
                    return Ok(Err("a pattern comparison of a non-string type"));
                };
                if comparison.holds(value.cmp(constant)) {
                    matched += 1;
                }
            }
            histogram = matched as f64 / distribution.histogram_len() as f64;
        }
        let selectivity = (1.0 - variable.null_fraction - common) * histogram + mcv_selectivity;
        Ok(Ok(clamp_probability(selectivity)))
    }

    /// `scalararraysel` of `column comparison ANY (elements)` (or `ALL`).
    pub(super) fn array_selectivity(
        &self,
        column: usize,
        comparison: Comparison,
        any: bool,
        elements: Option<&[Datum<'_>]>,
    ) -> Result<Result<f64, &'static str>, SqlError> {
        let equality = comparison == Comparison::Eq;
        let inequality = comparison == Comparison::Ne;
        let Some(elements) = elements else {
            // An array of unknown value: ten elements of unknown value.
            let element = match self.operator_selectivity(column, comparison, None)? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            };
            let mut selectivity = if any { 0.0 } else { 1.0 };
            for _ in 0..10 {
                selectivity = if any {
                    selectivity + element - selectivity * element
                } else {
                    selectivity * element
                };
            }
            return Ok(Ok(selectivity));
        };
        let mut selectivity = if any { 0.0 } else { 1.0 };
        let mut disjoint = selectivity;
        for &element in elements {
            let element = match self.operator_selectivity(column, comparison, Some(element))? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            };
            if any {
                selectivity = selectivity + element - selectivity * element;
                if equality {
                    disjoint += element;
                }
            } else {
                selectivity *= element;
                if inequality {
                    disjoint += element - 1.0;
                }
            }
        }
        if (if any { equality } else { inequality }) && (0.0..=1.0).contains(&disjoint) {
            selectivity = disjoint;
        }
        Ok(Ok(selectivity))
    }

    /// `clauselist_selectivity` of `restrictions`, with the table's
    /// extended statistics.
    pub(super) fn selectivity(
        &self,
        restrictions: &[clauses::Restriction<'a>],
    ) -> Result<f64, SqlError> {
        let mark = self.arena.mark();
        let result = extended::conditions(restrictions, self.arena)
            .and_then(|conditions| self.clauselist_ext(conditions));
        // SAFETY: the conditions were consumed above.
        unsafe { self.arena.rewind_to(mark) };
        result
    }

    /// `clauselist_selectivity` of clauses given as each one's range bound
    /// (`(column, lower)`, for a column compared by `<`, `<=`, `>` or `>=`)
    /// and selectivity, pairing each column's lower and upper bounds as
    /// `addRangeClause` does.
    pub(super) fn clauselist(
        &self,
        clauses: impl Iterator<Item = (Option<(usize, bool)>, f64)> + Clone,
    ) -> f64 {
        let mut single = clauses.clone();
        if let (Some(only), None) = (single.next(), single.next()) {
            return only.1;
        }
        let mut product = 1.0;
        // (column, lower bound, upper bound)
        let mut ranges = [(usize::MAX, None::<f64>, None::<f64>); MAX_RESTRICTIONS];
        let mut range_count = 0;
        for (bound, selectivity) in clauses {
            let Some((column, lower)) = bound.filter(|_| range_count < MAX_RESTRICTIONS) else {
                product *= selectivity;
                continue;
            };
            let at = match ranges[..range_count]
                .iter()
                .position(|range| range.0 == column)
            {
                Some(at) => at,
                None => {
                    ranges[range_count] = (column, None, None);
                    range_count += 1;
                    range_count - 1
                }
            };
            let slot = if lower {
                &mut ranges[at].1
            } else {
                &mut ranges[at].2
            };
            *slot = Some(slot.map_or(selectivity, |held| held.min(selectivity)));
        }
        // `rqlist` holds the most recently added column first.
        for &(column, low, high) in ranges[..range_count].iter().rev() {
            product *= match (low, high) {
                (Some(low), Some(high)) => {
                    if low == DEFAULT_INEQ_SEL || high == DEFAULT_INEQ_SEL {
                        DEFAULT_RANGE_INEQ_SEL
                    } else {
                        let mut selectivity = high + low - 1.0;
                        selectivity += self.null_selectivity(column, true);
                        if selectivity <= 0.0 {
                            if selectivity < -0.01 {
                                DEFAULT_RANGE_INEQ_SEL
                            } else {
                                1.0e-10
                            }
                        } else {
                            selectivity
                        }
                    }
                }
                (Some(bound), None) | (None, Some(bound)) => bound,
                (None, None) => 1.0,
            };
        }
        product
    }
}

/// Whether `prop` reads a key expression, numbered past the table's
/// columns.
fn prop_reads_expression(prop: &predicate::Prop<'_>, variables: &ExpressionVariables<'_>) -> bool {
    use predicate::Prop;
    match *prop {
        Prop::Compare { column, .. }
        | Prop::NullTest { column, .. }
        | Prop::Array { column, .. } => variables.get(column).is_some(),
        Prop::And(parts) | Prop::Or(parts) => parts
            .iter()
            .any(|part| prop_reads_expression(part, variables)),
        Prop::Opaque(_) => false,
    }
}

/// A value's and its histogram bin's positions on a common scale
/// (`convert_to_scalar`).
enum Scalar {
    Values(f64, f64, f64),
    /// PostgreSQL cannot place the type; it assumes the bin's middle.
    Unconvertible,
    /// PostgreSQL places the type in a way pos3ql does not model.
    OutsideModel,
}

fn numeric_scalar(value: &Datum<'_>) -> Option<f64> {
    Some(match *value {
        Datum::Bool(value) => f64::from(u8::from(value)),
        Datum::Int2(value) => f64::from(value),
        Datum::Int4(value) => f64::from(value),
        Datum::Int8(value) => value as f64,
        Datum::Float4(value) => f64::from(value),
        Datum::Float8(value) => value,
        Datum::Numeric(value) => value.to_f64(),
        Datum::Oid(value) => f64::from(value),
        _ => return None,
    })
}

fn time_scalar(value: &Datum<'_>) -> Option<f64> {
    const MICROS_PER_DAY: f64 = 86_400_000_000.0;
    Some(match *value {
        Datum::Timestamp(value) | Datum::Timestamptz(value) => value as f64,
        Datum::Date(day) => match day {
            i32::MIN => -f64::MAX,
            i32::MAX => f64::MAX,
            day => f64::from(day) * MICROS_PER_DAY,
        },
        Datum::Time(value) => value as f64,
        Datum::Timetz(time, zone) => time as f64 + f64::from(zone) * 1_000_000.0,
        Datum::Interval(interval) => {
            interval.micros as f64
                + f64::from(interval.days) * MICROS_PER_DAY
                + f64::from(interval.months) * ((365.25 / 12.0) * MICROS_PER_DAY)
        }
        _ => return None,
    })
}

fn string_bytes<'v>(value: &'v Datum<'_>) -> Option<&'v [u8]> {
    match value {
        Datum::Text(text) | Datum::Bpchar(text) => Some(text.as_bytes()),
        Datum::Char(byte) => Some(core::slice::from_ref(byte)),
        _ => None,
    }
}

fn scalar_positions(
    value: &Datum<'_>,
    low: &Datum<'_>,
    high: &Datum<'_>,
    collation: Collation,
) -> Scalar {
    if let (Some(value), Some(low), Some(high)) = (
        numeric_scalar(value),
        numeric_scalar(low),
        numeric_scalar(high),
    ) {
        return Scalar::Values(value, low, high);
    }
    if let (Some(value), Some(low), Some(high)) =
        (time_scalar(value), time_scalar(low), time_scalar(high))
    {
        return Scalar::Values(value, low, high);
    }
    if let (Some(value), Some(low), Some(high)) =
        (string_bytes(value), string_bytes(low), string_bytes(high))
    {
        if !c_collation(collation) {
            return Scalar::OutsideModel;
        }
        let (value, low, high) = string_scalars(value, low, high);
        return Scalar::Values(value, low, high);
    }
    if let (Datum::Bytea(value), Datum::Bytea(low), Datum::Bytea(high)) = (value, low, high) {
        let (value, low, high) = bytea_scalars(value, low, high);
        return Scalar::Values(value, low, high);
    }
    match value {
        Datum::Inet(_) | Datum::Cidr(_) | Datum::Macaddr(_) | Datum::Macaddr8(_) => {
            Scalar::OutsideModel
        }
        _ => Scalar::Unconvertible,
    }
}

/// `convert_string_to_scalar` of C-collated strings, each read to its first
/// NUL as PostgreSQL's C strings are.
fn string_scalars(value: &[u8], low: &[u8], high: &[u8]) -> (f64, f64, f64) {
    let cut = |text: &[u8]| -> usize {
        text.iter()
            .position(|&byte| byte == 0)
            .unwrap_or(text.len())
    };
    let (value, low, high) = (&value[..cut(value)], &low[..cut(low)], &high[..cut(high)]);
    let mut range_low = i32::from(high.first().copied().unwrap_or(0));
    let mut range_high = range_low;
    for &byte in low.iter().chain(high) {
        range_low = range_low.min(i32::from(byte));
        range_high = range_high.max(i32::from(byte));
    }
    for (first, last) in [(b'A', b'Z'), (b'a', b'z'), (b'0', b'9')] {
        let (first, last) = (i32::from(first), i32::from(last));
        if range_low <= last && range_high >= first {
            range_low = range_low.min(first);
            range_high = range_high.max(last);
        }
    }
    if range_high - range_low < 9 {
        range_low = i32::from(b' ');
        range_high = 127;
    }
    let mut prefix = 0;
    while prefix < low.len()
        && prefix < high.len()
        && prefix < value.len()
        && low[prefix] == high[prefix]
        && low[prefix] == value[prefix]
    {
        prefix += 1;
    }
    let one = |text: &[u8]| -> f64 {
        let text = &text[prefix.min(text.len())..];
        if text.is_empty() {
            return 0.0;
        }
        let base = f64::from(range_high - range_low + 1);
        let mut number = 0.0;
        let mut denominator = base;
        for &byte in text.iter().take(12) {
            let character = i32::from(byte).clamp(range_low - 1, range_high + 1);
            number += f64::from(character - range_low) / denominator;
            denominator *= base;
        }
        number
    };
    (one(value), one(low), one(high))
}

/// `convert_bytea_to_scalar`.
fn bytea_scalars(value: &[u8], low: &[u8], high: &[u8]) -> (f64, f64, f64) {
    let minimum = value.len().min(low.len()).min(high.len());
    let mut prefix = 0;
    while prefix < minimum && low[prefix] == high[prefix] && low[prefix] == value[prefix] {
        prefix += 1;
    }
    let one = |bytes: &[u8]| -> f64 {
        let bytes = &bytes[prefix..];
        let length = bytes.len().min(10);
        let mut number = 0.0;
        let mut denominator = 256.0;
        for &byte in &bytes[..length] {
            number += f64::from(byte) / denominator;
            denominator *= 256.0;
        }
        number
    };
    (one(value), one(low), one(high))
}

/// The most `ORDER BY` keys the model reads.
pub(crate) const MAX_ORDER_KEYS: usize = 8;

/// One `ORDER BY` key (a pathkey): a column of the relation with its
/// direction and NULLs placement, or an expression no index orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OrderKey {
    Column {
        column: usize,
        descending: bool,
        nulls_first: bool,
    },
    Expression,
}

/// What the query does with the scan's rows that its path depends on: its
/// ordering and a `LIMIT` (count, when given, and offset).
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueryShape<'a> {
    pub(crate) order: [OrderKey; MAX_ORDER_KEYS],
    pub(crate) order_len: usize,
    pub(crate) limit: Option<(Option<f64>, f64)>,
    /// The scan may stop once the limit has its rows: no row lock or later
    /// filter can drop one of them.
    pub(crate) stop_at_limit: bool,
    /// A part of the query the model does not read, which only a plan
    /// without index paths leaves exact.
    pub(crate) unmodeled: Option<&'static str>,
    /// The scan reads the target of an `UPDATE` or `DELETE`, whose
    /// conditions a partial index's predicate implies still filter its rows.
    pub(crate) target: bool,
    /// The query reads the table without its descendants (`ONLY`).
    pub(crate) only: bool,
    /// The columns the scan returns above it (`reltarget`), which set its
    /// width; `None` when they are the columns it reads.
    pub(crate) output: Option<ColumnSet>,
    /// Only whether PostgreSQL builds any index path matters: the answer is
    /// [`Planned::SeqOnly`] when it builds none.
    pub(crate) probe: bool,
    /// The target list the scan computes, when the model reads it: its
    /// width and the per-row cost of its expressions.
    pub(crate) projection: Option<Projection>,
    /// The scan is of one member of a partition or inheritance tree.
    pub(crate) member: Member<'a>,
    /// The width of the row identity a write's scan returns beside its
    /// columns: `ctid`, and `tableoid` for a tree's member.
    pub(crate) identity_width: u32,
    /// The query groups the relation's rows: the model chooses between
    /// sorting and hashing them (`add_paths_to_grouping_rel`).
    pub(crate) grouping: Option<GroupingShape>,
}

/// A `GROUP BY` of the relation's columns, aggregated above its scan.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupingShape {
    /// The grouping columns, in the order a sort for them would use.
    pub(crate) columns: [usize; MAX_ORDER_KEYS],
    pub(crate) len: usize,
    pub(crate) aggregates: AggregateCosts,
    /// The width of the rows the aggregation returns.
    pub(crate) output_width: f64,
    pub(crate) order: GroupOrder,
    /// The `HAVING` qualification's cost and selectivity.
    pub(crate) having: Option<aggregate::QualCosts>,
    /// Each key of `columns` as `estimate_num_groups` reads it, when a key
    /// is an expression; `None` reads every key as its column.
    pub(crate) variables: Option<[GroupVariable; MAX_ORDER_KEYS]>,
    /// The per-group cost of the grouping's target list
    /// (`create_agg_path`'s tlist evaluation).
    pub(crate) target_per_group: f64,
    /// The grouping is a `DISTINCT`, whose sorted strategy is a `Unique`
    /// that evaluates no target list.
    pub(crate) distinct: bool,
}

/// How the query's `ORDER BY` relates to its grouping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupOrder {
    None,
    /// The rows a sorted grouping returns are in the `ORDER BY` order.
    Grouped,
    /// The `ORDER BY` needs a sort of the groups whichever way they are
    /// formed.
    Other,
}

/// How the relation's groups are formed and ordered, with each step's
/// startup and total cost.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupedPath {
    /// A `HashAggregate`, rather than a `GroupAggregate`.
    pub(crate) hashed: bool,
    /// The query calls an aggregate: a sorted grouping without one is a
    /// `Group`.
    pub(crate) aggregated: bool,
    /// The grouping columns in the order a sorted aggregate keys them.
    pub(crate) keys: [usize; MAX_ORDER_KEYS],
    /// How many of them are keys: the others a condition fixes to a
    /// constant.
    pub(crate) key_count: usize,
    /// The sort of a `GroupAggregate`'s input by the grouping columns, when
    /// the scan does not return them ordered: its startup and total cost
    /// and how many leading keys the scan presorts (an incremental sort
    /// when not zero).
    pub(crate) input_sort: Option<(f64, f64, u8)>,
    /// The partitions a `HashAggregate` expects to spill its groups to.
    pub(crate) planned_partitions: Option<u32>,
    /// What sizes a `HashAggregate`'s memory, when the model covers it.
    pub(crate) hash_agg: Option<crate::sql::query::postgres_memory::HashAggSizing>,
    pub(crate) startup: f64,
    pub(crate) total: f64,
    pub(crate) rows: f64,
    /// The sort of the groups for the `ORDER BY`.
    pub(crate) output_sort: Option<(f64, f64)>,
    /// The `Limit` over them: its startup and total cost and its rows.
    pub(crate) limit: Option<(f64, f64, f64)>,
    /// The relation's cheapest path the grouping reads: its startup and
    /// total cost, rows and width.
    pub(crate) input: AggInput,
}

/// What the planning of one member of a partition or inheritance tree
/// returns to the tree's.
#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "a join scan is carried inline on the stack; boxing would heap-allocate"
)]
pub(crate) enum Member<'a> {
    /// The relation is planned on its own.
    No,
    /// Its paths, as [`Planned::Members`].
    Paths,
    /// The scan of the one of its paths the tree chose.
    Describe(paths::Candidate),
    /// A base relation of a join PostgreSQL's join search plans: its paths
    /// with their merge pathkeys and parameterizations, or the scan of one.
    Join(JoinScan<'a>),
}

/// What the paths of one base relation of a join depend on beyond its own
/// conditions (`create_index_paths` with its join clauses).
#[derive(Clone, Copy, Debug)]
pub(crate) struct JoinScan<'a> {
    /// The join clauses an index of the relation may apply, as
    /// `generate_implied_equalities_for_column` derives them: the last
    /// conjuncts of the scan's condition, each equating one of its columns
    /// with another relation's: `(class, other relation)`, each class's in
    /// the order of its members.
    pub(crate) params: &'a [(u8, u8)],
    /// Whether each parameter is a join clause of the relation's
    /// `joininfo` (`match_join_clauses_to_index`), applied on its own,
    /// rather than an equality a class implies, of which a scan applies one.
    pub(crate) clause_params: &'a [bool],
    /// The other relations each parameter reads beyond its first (an
    /// expression of several relations') and the outer joins that null
    /// them, which a path applying it is parameterized by too.
    pub(crate) param_others: &'a [joins::Relids],
    /// Each column's equivalence class when it has a member in another
    /// relation (`eclass_useful_for_merging`) or the query orders by it,
    /// else [`NO_CLASS`].
    pub(crate) classes: [u8; 64],
    /// The classes useful for merging, by bit.
    pub(crate) merging: u64,
    /// The query's pathkeys (`query_pathkeys`).
    pub(crate) order: joins::Keys,
    /// Each relation's rows by FROM position (`get_loop_count`).
    pub(crate) rel_rows: &'a [f64],
    /// `root->total_table_pages`.
    pub(crate) total_table_pages: f64,
    /// The path to describe, or `None` for the relation's paths.
    pub(crate) describe: Option<JoinDescribe>,
}

/// Which path of a join's base relation a scan runs: its kind, index and
/// direction, and parameterization, which name it whatever join clauses
/// its planning sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JoinDescribe {
    pub(crate) kind: ScanKind,
    pub(crate) index: u16,
    pub(crate) backward: bool,
    pub(crate) param: joins::Relids,
}

/// No equivalence class in [`JoinScan::classes`].
pub(crate) const NO_CLASS: u8 = u8::MAX;

/// The scan's target list (`apply_scanjoin_target_to_paths`): the select
/// list and the sort keys it lacks, each column's analyzed width and each
/// expression's type width (`set_pathtarget_cost_width`), and the per-row
/// cost of evaluating its expressions, which the scan's cost includes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Projection {
    pub(crate) width: u32,
    pub(crate) per_tuple: f64,
    /// The width of its entries that are not columns of the relation.
    pub(crate) expression_width: u32,
    /// How often each of the relation's first 64 columns is an entry.
    pub(crate) columns: [u8; 64],
    /// An entry is a column past the 64th.
    pub(crate) wide: bool,
    /// Every entry is a distinct column of the relation: the list is the
    /// relation's own (`scanjoin_target_same_exprs`).
    pub(crate) plain: bool,
}

impl Projection {
    /// The list's width with each column `column_width` wide.
    pub(super) fn width_with(&self, column_width: &dyn Fn(usize) -> f64) -> u32 {
        let mut width = f64::from(self.expression_width);
        for (column, &count) in self.columns.iter().enumerate() {
            width += f64::from(count) * column_width(column);
        }
        width as u32
    }
}

impl Default for QueryShape<'_> {
    fn default() -> Self {
        Self {
            order: [OrderKey::Expression; MAX_ORDER_KEYS],
            order_len: 0,
            limit: None,
            stop_at_limit: false,
            unmodeled: None,
            target: false,
            only: false,
            output: None,
            probe: false,
            projection: None,
            member: Member::No,
            identity_width: 0,
            grouping: None,
        }
    }
}

impl QueryShape<'_> {
    pub(crate) fn order(&self) -> &[OrderKey] {
        &self.order[..self.order_len]
    }
}

/// The width PostgreSQL's planner gives `column` of the table in `slot` in
/// a target list (`get_attavgwidth`): its analyzed width, or its type's.
pub(crate) fn column_width(storage: &Storage, txid: u32, slot: usize, column: usize) -> u32 {
    output_width(
        &storage.table_def(slot, txid),
        &storage.table_statistics(slot, txid),
        Some(ColumnSet::from_column(column)),
    )
}

/// The width of the columns the scan returns (`set_rel_width`): each
/// column's analyzed width, or its type's.
pub(super) fn output_width(
    definition: &TableDef,
    statistics: &TableStatistics,
    needed: Option<ColumnSet>,
) -> u32 {
    let mut width = 0.0;
    for (column, meta) in definition.columns().iter().enumerate() {
        if needed.is_some_and(|needed| !needed.contains(column)) {
            continue;
        }
        width += definition
            .attributes
            .number(column)
            .and_then(|attribute| statistics.column(attribute))
            .filter(|column| column.average_width > 0)
            .map_or_else(
                || type_average_width(meta.ctype, meta.type_mod),
                |column| f64::from(column.average_width),
            );
    }
    width as u32
}

/// `index_pages_fetched` (the Mackert and Lohman formula).
pub(super) fn index_pages_fetched(
    tuples: f64,
    pages: f64,
    total_pages: f64,
    effective_cache_size: f64,
) -> f64 {
    let table_pages = if pages > 1.0 { pages } else { 1.0 };
    let mut cache = effective_cache_size * table_pages / total_pages;
    cache = if cache <= 1.0 { 1.0 } else { cache.ceil() };
    if table_pages <= cache {
        let fetched = (2.0 * table_pages * tuples) / (2.0 * table_pages + tuples);
        if fetched >= table_pages {
            table_pages
        } else {
            fetched.ceil()
        }
    } else {
        let limit = (2.0 * table_pages * cache) / (2.0 * table_pages - cache);
        let fetched = if tuples <= limit {
            (2.0 * table_pages * tuples) / (2.0 * table_pages + tuples)
        } else {
            cache + (tuples - limit) * (table_pages - cache) / table_pages
        };
        fetched.ceil()
    }
}

/// The shape of a query over one relation that its scan path depends on,
/// or the shape outside the model.
#[expect(clippy::too_many_arguments, reason = "the query's parts are explicit")]
pub(crate) fn query_shape<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    items: &'a [crate::sql::ast::SelectItem<'a>],
    order_by: &'a [crate::sql::ast::OrderBy<'a>],
    limit: Option<&'a Expr<'a>>,
    offset: Option<&'a Expr<'a>>,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<QueryShape<'a>, SqlError> {
    let mut shape = QueryShape::default();
    let variables = if order_by.is_empty() {
        None
    } else {
        query_variables(storage, txid, scope, arena)?
    };
    for order in order_by {
        let target = super::resolve_order_target(
            order.expression,
            items,
            scope,
            arena,
            storage,
            txid,
            None,
        )?;
        // A constant key orders nothing (its pathkey is redundant).
        if target.is_constant(false) && !target.calls_routine() {
            continue;
        }
        let target = without_own_type_cast(target, scope);
        let key = match target {
            Expr::Column { qualifier, name } => match scope.find_column(*qualifier, name) {
                Ok(ResolvedColumn::Table(0, column)) => OrderKey::Column {
                    column,
                    descending: order.descending,
                    nulls_first: order.nulls_first,
                },
                _ => OrderKey::Expression,
            },
            // An expression an index has as a key orders as that key.
            _ => match variables {
                Some(variables) => match variables.find(scope, target, arena)? {
                    Some(column) => OrderKey::Column {
                        column,
                        descending: order.descending,
                        nulls_first: order.nulls_first,
                    },
                    None => OrderKey::Expression,
                },
                None => OrderKey::Expression,
            },
        };
        // A key on a column an earlier key orders is redundant.
        if let OrderKey::Column { column, .. } = key
            && shape.order().iter().any(|earlier| {
                matches!(earlier, OrderKey::Column { column: other, .. } if *other == column)
            })
        {
            continue;
        }
        if shape.order_len == MAX_ORDER_KEYS {
            shape.unmodeled = Some("an ORDER BY of more than eight keys");
            break;
        }
        shape.order[shape.order_len] = key;
        shape.order_len += 1;
    }
    let constant = |expression: Option<&'a Expr<'a>>| -> Result<Option<Option<f64>>, SqlError> {
        let Some(expression) = expression else {
            return Ok(Some(None));
        };
        if !expression.is_constant(false) || expression.calls_routine() {
            return Ok(None);
        }
        Ok(Some(
            match crate::sql::eval::eval(expression, arena, params, &crate::sql::eval::NoColumns) {
                Ok(Datum::Null) => None,
                Ok(value) => numeric_scalar(&value),
                Err(_) => return Ok(None),
            },
        ))
    };
    shape.projection = projection(storage, txid, scope, items, order_by, arena)?;
    let (Some(count), Some(skip)) = (constant(limit)?, constant(offset)?) else {
        shape.unmodeled = Some("a LIMIT or OFFSET that is not a constant");
        return Ok(shape);
    };
    shape.limit = (limit.is_some() || offset.is_some()).then_some((count, skip.unwrap_or(0.0)));
    Ok(shape)
}

/// A `LIMIT`'s rows, if any, and its `OFFSET`'s, when either is present.
pub(crate) type LimitEstimate = Option<(Option<f64>, f64)>;

/// A query's `LIMIT` and `OFFSET` as the planner estimates them
/// (`preprocess_limit`): `Some(None)` without either, `None` when one is
/// not a constant.
pub(crate) fn constant_limit<'a>(
    limit: Option<&'a Expr<'a>>,
    offset: Option<&'a Expr<'a>>,
    arena: &'a Arena,
) -> Result<Option<LimitEstimate>, SqlError> {
    let constant = |expression: Option<&'a Expr<'a>>| -> Option<Option<f64>> {
        let Some(expression) = expression else {
            return Some(None);
        };
        if !expression.is_constant(false) || expression.calls_routine() {
            return None;
        }
        Some(
            match crate::sql::eval::eval(expression, arena, &[], &crate::sql::eval::NoColumns) {
                Ok(Datum::Null) => None,
                Ok(value) => numeric_scalar(&value),
                Err(_) => return None,
            },
        )
    };
    let (Some(count), Some(skip)) = (constant(limit), constant(offset)) else {
        return Ok(None);
    };
    Ok(Some(
        (limit.is_some() || offset.is_some()).then_some((count, skip.unwrap_or(0.0))),
    ))
}

/// The target list a scan of the one plain table `scope` reads computes
/// for `items` ordered by `order_by`; `None` for a list the model does not
/// read (a record expansion, or an expression of a catalog-defined type).
fn projection<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    items: &'a [crate::sql::ast::SelectItem<'a>],
    order_by: &'a [crate::sql::ast::OrderBy<'a>],
    arena: &'a Arena,
) -> Result<Option<Projection>, SqlError> {
    use crate::sql::ast::SelectItem;
    if scope.n != 1 || scope.slots[0] == usize::MAX {
        return Ok(None);
    }
    let Some(definition) = scope.defs.stored(0) else {
        return Ok(None);
    };
    let statistics = storage.table_statistics(scope.slots[0], txid);
    let mut width = 0u32;
    let mut per_tuple = 0.0;
    let mut expression_width = 0u32;
    let mut columns = [0u8; 64];
    let mut wide = false;
    let mut plain = true;
    fn count_column(columns: &mut [u8; 64], plain: &mut bool, wide: &mut bool, column: usize) {
        match columns.get_mut(column) {
            Some(count) => {
                *count = count.saturating_add(1);
                *plain &= *count == 1;
            }
            None => *wide = true,
        }
    }
    let entry = |expression: &'a Expr<'a>| {
        target_entry(
            storage,
            txid,
            scope,
            definition,
            &statistics,
            expression,
            arena,
        )
    };
    for item in items {
        match item {
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
                width += output_width(definition, &statistics, None);
                for column in 0..definition.n_columns {
                    count_column(&mut columns, &mut plain, &mut wide, column);
                }
            }
            SelectItem::Expr { expression, .. } => {
                let Some((entry_width, cost, column)) = entry(expression) else {
                    return Ok(None);
                };
                width += entry_width;
                per_tuple += cost;
                match column {
                    Some(column) => count_column(&mut columns, &mut plain, &mut wide, column),
                    None => {
                        plain = false;
                        expression_width += entry_width;
                    }
                }
            }
            SelectItem::RecordStar(_) => return Ok(None),
        }
    }
    // A sort key the select list lacks joins the target list unnamed.
    for order in order_by {
        let target = super::resolve_order_target(
            order.expression,
            items,
            scope,
            arena,
            storage,
            txid,
            None,
        )?;
        let listed = items.iter().any(|item| match item {
            SelectItem::Expr { expression, .. } => **expression == *target,
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
                matches!(target, Expr::Column { .. })
            }
            SelectItem::RecordStar(_) => false,
        });
        if listed || (target.is_constant(false) && !target.calls_routine()) {
            continue;
        }
        let Some((entry_width, cost, column)) = entry(target) else {
            return Ok(None);
        };
        width += entry_width;
        per_tuple += cost;
        match column {
            Some(column) => count_column(&mut columns, &mut plain, &mut wide, column),
            None => {
                plain = false;
                expression_width += entry_width;
            }
        }
    }
    Ok(Some(Projection {
        width,
        per_tuple,
        expression_width,
        columns,
        wide,
        plain,
    }))
}

/// The target list a grouping of the one table of `scope` reads from its
/// scan (`make_group_input_target`): each grouping key `keys` holds, then
/// every column the other expressions `others` read (the select list,
/// `HAVING` and `ORDER BY` entries that are no key, aggregate arguments
/// included). `None` when an entry is outside the model.
pub(crate) fn grouping_projection<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    keys: &[&'a Expr<'a>],
    others: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<Option<Projection>, SqlError> {
    let Some(definition) = scope.defs.stored(0) else {
        return Ok(None);
    };
    let statistics = storage.table_statistics(scope.slots[0], txid);
    let mut projection = Projection {
        width: 0,
        per_tuple: 0.0,
        expression_width: 0,
        columns: [0; 64],
        wide: false,
        plain: false,
    };
    let mut listed = ColumnSet::EMPTY;
    let mut add_column = |projection: &mut Projection, column: usize| {
        if listed.contains(column) {
            return;
        }
        listed.insert(column);
        projection.width += output_width(
            definition,
            &statistics,
            Some(ColumnSet::from_column(column)),
        );
        match projection.columns.get_mut(column) {
            Some(count) => *count = count.saturating_add(1),
            None => projection.wide = true,
        }
    };
    for &key in keys {
        let Some((width, cost, column)) =
            target_entry(storage, txid, scope, definition, &statistics, key, arena)
        else {
            return Ok(None);
        };
        match column {
            Some(column) => add_column(&mut projection, column),
            None => {
                projection.width += width;
                projection.per_tuple += cost;
                projection.expression_width += width;
            }
        }
    }
    for other in others {
        let mut foreign = false;
        let mut read = ColumnSet::EMPTY;
        other.for_each_column_reference(&mut |qualifier, name| match scope
            .find_column(qualifier, name)
        {
            Ok(ResolvedColumn::Table(0, column)) => read.insert(column),
            _ => foreign = true,
        });
        if foreign {
            return Ok(None);
        }
        for column in read.iter() {
            add_column(&mut projection, column);
        }
    }
    Ok(Some(projection))
}

/// `expression` without a cast of a column of the one relation of `scope`
/// to the column's own type, which PostgreSQL's parser drops.
pub(super) fn without_own_type_cast<'a>(
    expression: &'a Expr<'a>,
    scope: &QueryScope<'_>,
) -> &'a Expr<'a> {
    let Expr::Cast {
        operand,
        type_name,
        type_mod: -1,
        ..
    } = expression
    else {
        return expression;
    };
    let Expr::Column { qualifier, name } = operand else {
        return expression;
    };
    let (Ok(ResolvedColumn::Table(0, column)), Some(definition)) =
        (scope.find_column(*qualifier, name), scope.defs.stored(0))
    else {
        return expression;
    };
    let meta = &definition.columns()[column];
    let target = ColType::from_sql_name(type_name.trim_start_matches("pg_catalog."));
    if meta.user_type.is_none() && target == Some(meta.ctype) {
        operand
    } else {
        expression
    }
}

/// A target list entry's width and per-row evaluation cost, and the
/// column of the relation it is; `None` for an entry the model does not
/// read (a subquery, or an expression of a catalog-defined type).
fn target_entry<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    definition: &TableDef,
    statistics: &TableStatistics,
    expression: &'a Expr<'a>,
    arena: &'a Arena,
) -> Option<(u32, f64, Option<usize>)> {
    let costs = Costs::current();
    let expression = without_own_type_cast(expression, scope);
    if let Expr::Column { qualifier, name } = expression
        && let Ok(ResolvedColumn::Table(0, column)) = scope.find_column(*qualifier, name)
    {
        return Some((
            output_width(definition, statistics, Some(ColumnSet::from_column(column))),
            0.0,
            Some(column),
        ));
    }
    // An uncorrelated scalar subquery is its InitPlan's output parameter,
    // which costs nothing to read (`cost_qual_eval_walker`).
    let subquery_parameter = match expression {
        Expr::Subquery(select) => {
            !crate::sql::query::select_correlated(select, storage, txid, arena).ok()?
        }
        _ => false,
    };
    // A correlated scalar subquery is a subplan run per row: its recorded
    // `cost_subplan` charge (`cost_qual_eval_walker`).
    let subplan = match expression {
        Expr::Subquery(_) if !subquery_parameter => {
            match crate::sql::query::subplan_qual_cost(expression)? {
                (0.0, per_tuple) => Some(per_tuple),
                _ => return None,
            }
        }
        _ => None,
    };
    if expression.contains_subquery() && !subquery_parameter && subplan.is_none() {
        return None;
    }
    let (type_oid, type_mod) =
        crate::sql::exec::infer_type_catalog(expression, Some(definition), storage, txid, arena)
            .ok()?;
    let Some((ctype, None)) = crate::sql::exec::catalog_column_type(storage, txid, type_oid) else {
        return None;
    };
    let columns = crate::sql::exec::AliasedDefCols {
        definition: crate::storage::RelationDefinition::from_definition(definition),
        alias: None,
        output_aliases: &[],
        remote_ctid: false,
    };
    Some((
        type_average_width(ctype, i32::from(type_mod)) as u32,
        if let Some(per_tuple) = subplan {
            per_tuple
        } else if subquery_parameter {
            0.0
        } else {
            super::plan::qual_cost(expression, &columns, storage, txid, arena) / 2.0
                * costs.cpu_operator
        },
        None,
    ))
}

/// The key expressions of the indexes of the one plain table `scope` reads.
fn query_variables<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    arena: &'a Arena,
) -> Result<Option<ExpressionVariables<'a>>, SqlError> {
    if scope.n != 1 || scope.slots[0] == usize::MAX {
        return Ok(None);
    }
    let Some(definition) = scope.defs.stored(0) else {
        return Ok(None);
    };
    let slot = scope.slots[0];
    let Some(catalog) = paths::catalog_indexes(storage, txid, slot, arena)? else {
        return Ok(None);
    };
    let statistics = storage.table_statistics(slot, txid);
    Ok(Some(
        expressions::table_expressions(
            storage,
            txid,
            slot,
            definition,
            &statistics,
            catalog,
            arena,
        )?
        .0,
    ))
}

/// The shape and read columns of the query whose scan of one relation is
/// running, which that scan's path depends on.
#[derive(Clone, Copy)]
struct ScanContext {
    slot: usize,
    needed: Option<ColumnSet>,
    shape: QueryShape<'static>,
}

thread_local! {
    static SCAN_CONTEXT: core::cell::Cell<Option<ScanContext>> =
        const { core::cell::Cell::new(None) };
}

/// Restores the enclosing query's context when a query's execution ends.
pub(crate) struct ScanContextScope(Option<ScanContext>);

impl Drop for ScanContextScope {
    fn drop(&mut self) {
        SCAN_CONTEXT.with(|context| context.set(self.0));
    }
}

/// Runs the rest of a query reading table `slot` under `condition` with
/// `shape`, its scan reading the columns `needed`, until the returned scope
/// ends.
pub(crate) fn enter_query(
    slot: usize,
    condition: Option<&Expr<'_>>,
    needed: Option<ColumnSet>,
    shape: Option<QueryShape<'_>>,
) -> ScanContextScope {
    let _ = condition;
    let context = shape.map(|shape| ScanContext {
        slot,
        needed,
        shape: QueryShape {
            member: match shape.member {
                Member::No => Member::No,
                Member::Paths => Member::Paths,
                Member::Describe(candidate) => Member::Describe(candidate),
                // A join's base relation runs the path its plan describes,
                // planned from the join's shape, never a running query's.
                Member::Join(_) => unreachable!("a join's base relation runs its described path"),
            },
            ..shape
        },
    });
    ScanContextScope(SCAN_CONTEXT.with(|current| current.replace(context)))
}

/// The shape and read columns of the running query when its scan of table
/// `slot` under `condition` is this one: the scan path depends on them.
pub(crate) fn running_query(
    slot: usize,
    condition: Option<&Expr<'_>>,
) -> Option<(QueryShape<'static>, Option<ColumnSet>)> {
    let context = SCAN_CONTEXT.with(core::cell::Cell::get)?;
    let _ = condition;
    (context.slot == slot).then_some((context.shape, context.needed))
}
