//! PostgreSQL 18's choice of access path for a query reading one plain
//! table: the sequential scan, each btree index's index or index-only scan
//! and the bitmap heap scan, costed as `costsize.c` costs them
//! (`cost_seqscan`, `cost_index` with `btcostestimate` and
//! `genericcostestimate`, `cost_bitmap_heap_scan`), with the selectivities
//! of `selfuncs.c` and `clausesel.c` from the statistics ANALYZE gathers,
//! and kept or discarded as `add_path` and `set_cheapest` do. EXPLAIN shows
//! the chosen path and execution returns its rows in that path's order.
//!
//! The model covers single-column plain btree indexes and restrictions
//! that compare a column with a constant (`=`, `<>`, `<`, `<=`, `>`, `>=`)
//! or test it for NULL. A table or query outside it gets no path here.

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, Collation, Expr, IndexAccessMethod};
use crate::sql::catalog::PlannerIndex;
use crate::sql::eval::SqlError;
use crate::sql::types::{ColType, Datum};
use crate::storage::{ColumnDistribution, MAX_COLUMNS, Storage, TableDef, TableStatistics};

use super::scope::{QueryScope, ResolvedColumn};

/// `selfuncs.h`' default selectivities.
const DEFAULT_INEQ_SEL: f64 = 0.333_333_333_333_333_3;
const DEFAULT_RANGE_INEQ_SEL: f64 = 0.005;
const DEFAULT_NUM_DISTINCT: f64 = 200.0;
const DEFAULT_UNK_SEL: f64 = 0.005;
const DEFAULT_NOT_UNK_SEL: f64 = 1.0 - DEFAULT_UNK_SEL;
/// `add_path`'s `STD_FUZZ_FACTOR`.
const FUZZ_FACTOR: f64 = 1.01;
const MAX_RESTRICTIONS: usize = 32;
const MAX_INDEXES: usize = 32;
const MAX_PATHS: usize = 2 * MAX_INDEXES + 2;

/// The planner's cost settings.
#[derive(Clone, Copy)]
struct Costs {
    seq_page: f64,
    random_page: f64,
    cpu_tuple: f64,
    cpu_index_tuple: f64,
    cpu_operator: f64,
    effective_cache_size: f64,
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

/// `clamp_row_est`.
pub(crate) fn clamp_row_est(rows: f64) -> f64 {
    if rows <= 1.0 { 1.0 } else { rows.round() }
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
fn type_average_width(ctype: ColType, type_mod: i32) -> f64 {
    let layout = ctype.physical_layout();
    if layout.length > 0 {
        return f64::from(layout.length);
    }
    // `type_maximum_size` of a length-limited character type: four bytes
    // per UTF-8 character plus the header.
    let maximum = match ctype {
        ColType::Varchar | ColType::Bpchar if type_mod > 4 => (type_mod - 4) * 4 + 4,
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
        tuples: (density * curpages).round(),
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
}

pub(crate) fn btree_size(
    entries: f64,
    key_width: f64,
    distinct: f64,
    deduplicate: bool,
) -> BtreeSize {
    let key = (key_width.ceil() as usize).next_multiple_of(8);
    let tuple = 8 + key;
    // Leaf bytes: a posting tuple per run of equal keys, else one tuple each.
    let entries = entries.max(0.0);
    let leaf_bytes = if deduplicate && distinct >= 1.0 && entries / distinct >= 2.0 {
        let per_key = entries / distinct;
        let per_posting = ((812 - tuple) / 6).max(1) as f64;
        let postings = (per_key / per_posting).ceil();
        let posting_bytes = (tuple as f64 + 6.0 * per_key / postings).ceil();
        distinct * postings * (((posting_bytes as usize).next_multiple_of(8) + 4) as f64)
    } else {
        entries * (tuple + 4) as f64
    };
    // Free bytes of a page after its header, special space and a line
    // pointer, less the fill factor's reserve and the high key.
    let leaf_capacity = (8148 - 819 - (tuple + 4)) as f64;
    let leaves = (leaf_bytes / leaf_capacity).ceil().max(1.0);
    let pivot = (tuple + 4) as f64;
    let inner_capacity = ((8148 - 2457) as f64 / pivot).floor().max(2.0);
    let mut level = leaves;
    let mut pages = 1.0 + leaves;
    let mut height = 0;
    while level > 1.0 {
        level = (level / inner_capacity).ceil();
        pages += level;
        height += 1;
    }
    BtreeSize { pages, height }
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

/// Whether `collation` orders strings byte by byte: the database default
/// (`C`) and the built-in collations do.
fn c_collation(collation: Collation) -> bool {
    !matches!(collation, Collation::Catalog(_))
}

/// A comparison operator with the column on its left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Comparison {
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

/// One restriction of the relation, with its place in PostgreSQL's model.
#[derive(Clone, Copy)]
struct Restriction<'a> {
    source: &'a Expr<'a>,
    shape: Shape<'a>,
    selectivity: f64,
    /// `cost_qual_eval`'s per-row cost.
    cost: f64,
}

#[derive(Clone, Copy)]
enum Shape<'a> {
    /// `column op constant`, the column side and the constant side.
    Compare {
        column: usize,
        comparison: Comparison,
        /// `None` for a parameter of a generic plan, whose value is unknown.
        constant: Option<Datum<'a>>,
        key: &'a Expr<'a>,
        operand: &'a Expr<'a>,
    },
    NullTest {
        column: usize,
        is_null: bool,
        key: &'a Expr<'a>,
    },
}

impl Shape<'_> {
    fn column(&self) -> usize {
        match *self {
            Self::Compare { column, .. } | Self::NullTest { column, .. } => column,
        }
    }

    /// Whether a btree on its column can use it as an index condition.
    fn indexable(&self) -> bool {
        match self {
            Self::Compare { comparison, .. } => *comparison != Comparison::Ne,
            Self::NullTest { .. } => true,
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

/// An index condition: the conjunct it came from, the indexed side, the
/// operator with the indexed side on its left, and the other side.
#[derive(Clone, Copy)]
pub(crate) struct IndexCondition<'a> {
    pub(crate) source: &'a Expr<'a>,
    pub(crate) key: &'a Expr<'a>,
    pub(crate) operator: Option<BinaryOp>,
    pub(crate) operand: Option<&'a Expr<'a>>,
}

/// The path PostgreSQL chose for the relation.
#[derive(Clone, Copy)]
pub(crate) struct ScanPath<'a> {
    pub(crate) kind: ScanKind,
    pub(crate) index_oid: i32,
    pub(crate) index_name: crate::util::StackStr<64>,
    /// The index key column, and whether the index stores it descending.
    pub(crate) index_column: usize,
    pub(crate) descending: bool,
    pub(crate) nulls_first: bool,
    conditions: [Option<IndexCondition<'a>>; MAX_RESTRICTIONS],
    condition_count: usize,
    /// The estimated rows the scan returns and their width.
    pub(crate) rows: f64,
    pub(crate) width: u32,
    pub(crate) startup_cost: f64,
    pub(crate) total_cost: f64,
    /// A bitmap heap scan's index scan: its cost and the rows it finds.
    pub(crate) index_cost: f64,
    pub(crate) index_rows: f64,
    /// The index scan reads its index backward.
    pub(crate) backward: bool,
    /// A sort above the scan orders its rows; otherwise they come in the
    /// requested order, or none was requested.
    pub(crate) sort: bool,
    /// Equal keys share posting lists, which a scan in either direction
    /// reads in row identity order.
    pub(crate) deduplicated: bool,
    /// The rows a `LIMIT` above an ordered scan takes before it stops.
    pub(crate) stop_after: Option<u64>,
    /// A unique index's equality reaches at most one row, whose order
    /// nothing else decides.
    pub(crate) single_row: bool,
}

impl<'a> ScanPath<'a> {
    /// The index conditions in index column order, each in the order the
    /// query wrote them.
    pub(crate) fn conditions(&self) -> impl Iterator<Item = IndexCondition<'a>> + '_ {
        self.conditions[..self.condition_count]
            .iter()
            .flatten()
            .copied()
    }
}

/// A column's statistics as `examine_variable` finds them.
struct VariableStatistics<'s> {
    /// Whether ANALYZE recorded the column (`statsTuple`).
    analyzed: bool,
    null_fraction: f64,
    distinct: f64,
    distribution: Option<ColumnDistribution<'s>>,
    ctype: ColType,
    collation: Collation,
    unique: bool,
    /// A btree index leads with the column (`get_actual_variable_range`).
    indexed: bool,
}

struct Planner<'s, 'a> {
    storage: &'s Storage,
    txid: u32,
    slot: usize,
    definition: &'s TableDef,
    statistics: &'s TableStatistics,
    size: RelationSize,
    costs: Costs,
    indexes: &'s [PlannerIndex],
    arena: &'a Arena,
}

impl<'s, 'a> Planner<'s, 'a> {
    fn variable(&self, column: usize) -> VariableStatistics<'s> {
        let meta = &self.definition.columns()[column];
        let attribute = self.definition.attributes.number(column);
        let analyzed = attribute.and_then(|attribute| self.statistics.column(attribute));
        let unique = self.indexes.iter().any(|index| {
            index.unique && index.n_cols == 1 && usize::from(index.columns[0]) == column
        });
        let indexed = self
            .indexes
            .iter()
            .any(|index| index.n_cols >= 1 && usize::from(index.columns[0]) == column);
        VariableStatistics {
            analyzed: analyzed.is_some(),
            null_fraction: analyzed.map_or(0.0, |column| f64::from(column.null_fraction)),
            distinct: analyzed.map_or(0.0, |column| {
                f64::from(crate::storage::column_distinct_catalog_value(meta, column))
            }),
            distribution: attribute.and_then(|attribute| self.statistics.distribution(attribute)),
            ctype: meta.ctype,
            collation: meta.collation,
            unique,
            indexed,
        }
    }

    /// `get_variable_numdistinct`.
    fn distinct(&self, variable: &VariableStatistics<'_>) -> (f64, bool) {
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
        if tuples < DEFAULT_NUM_DISTINCT {
            return (clamp_row_est(tuples), true);
        }
        (DEFAULT_NUM_DISTINCT, true)
    }

    fn compare(
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
    fn histogram_selectivity(
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
        let mut observe = |value: Datum<'_>| {
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
        let _ = scan.for_each_overlay(&mut |rowid, home| {
            self.storage
                .with_row_bytes(self.slot, rowid, home, |bytes| {
                    let mut values = [Datum::Null; MAX_COLUMNS];
                    crate::storage::rowenc::decode(bytes, schema, &mut values)?;
                    observe(values[column]);
                    Ok(())
                })?;
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        scan.for_each_spilled_row_batch(self.arena, true, None, &mut |batch| {
            for row in batch {
                let mut values = [Datum::Null; MAX_COLUMNS];
                let value = match row.representation {
                    crate::storage::SpilledRowRepresentation::Encoded(bytes) => {
                        crate::storage::rowenc::decode(bytes, schema, &mut values)?;
                        values[column]
                    }
                    crate::storage::SpilledRowRepresentation::Values(values) => values[column],
                };
                observe(value);
            }
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

    /// `nulltestsel`.
    fn null_selectivity(&self, column: usize, is_null: bool) -> f64 {
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

    /// `clauselist_selectivity`, pairing each column's lower and upper
    /// bounds as `addRangeClause` does.
    fn selectivity(&self, restrictions: &[Restriction<'_>]) -> f64 {
        if restrictions.len() == 1 {
            return restrictions[0].selectivity;
        }
        let mut product = 1.0;
        // (column, lower bound, upper bound)
        let mut ranges = [(usize::MAX, None::<f64>, None::<f64>); MAX_RESTRICTIONS];
        let mut range_count = 0;
        for restriction in restrictions {
            let bound = match restriction.shape {
                Shape::Compare {
                    column, comparison, ..
                } => match comparison {
                    Comparison::Lt | Comparison::Le => Some((column, false)),
                    Comparison::Gt | Comparison::Ge => Some((column, true)),
                    _ => None,
                },
                Shape::NullTest { .. } => None,
            };
            let Some((column, lower)) = bound else {
                product *= restriction.selectivity;
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
            *slot = Some(slot.map_or(restriction.selectivity, |held| {
                held.min(restriction.selectivity)
            }));
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
    // `convert_string_to_scalar` stops at the low bound's end.
    if prefix == low.len() || prefix < low.len() {
        // fallthrough: the loop above already stopped at the first
        // difference or the shortest string.
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

/// The single ordering column a query asks for: `ORDER BY column` with its
/// direction and NULLs placement.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestedOrder {
    pub(crate) column: usize,
    pub(crate) descending: bool,
    pub(crate) nulls_first: bool,
}

/// What the query does with the scan's rows that its path depends on: one
/// ordering column and a `LIMIT` (count, when given, and offset).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct QueryShape {
    pub(crate) order: Option<RequestedOrder>,
    pub(crate) limit: Option<(Option<f64>, f64)>,
    /// The scan may stop once the limit has its rows: no row lock or later
    /// filter can drop one of them.
    pub(crate) stop_at_limit: bool,
}

/// `cost_sort` of `rows` rows of `width` bytes whose input costs
/// `input_total`, in memory: a bounded heap sort for a limit under half the
/// rows, else a quicksort; `None` when the rows exceed `work_mem`.
fn sort_cost(
    input_total: f64,
    rows: f64,
    width: f64,
    limit: Option<(Option<f64>, f64)>,
    costs: Costs,
) -> Option<(f64, f64)> {
    let tuples = rows.max(2.0);
    let comparison = 2.0 * costs.cpu_operator;
    let limit_tuples = limit
        .and_then(|(count, offset)| count.map(|count| count + offset))
        .filter(|&limit| limit > 0.0 && limit < tuples);
    let output_tuples = limit_tuples.unwrap_or(tuples);
    let input_bytes = tuples * ((width as usize).next_multiple_of(8) + 24) as f64;
    let output_bytes = if output_tuples < tuples {
        input_bytes * output_tuples / tuples
    } else {
        input_bytes
    };
    let memory = f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0;
    if output_bytes > memory {
        return None;
    }
    let startup = if tuples > 2.0 * output_tuples || input_bytes > memory {
        comparison * tuples * (2.0 * output_tuples).log2()
    } else {
        comparison * tuples * tuples.log2()
    };
    let startup = input_total + startup;
    Some((startup, startup + costs.cpu_operator * tuples))
}

/// One candidate path.
#[derive(Clone, Copy)]
struct Candidate {
    kind: ScanKind,
    index: usize,
    startup: f64,
    total: f64,
    disabled: u32,
    /// The index path's `indextotalcost` and selectivity, for its bitmap.
    index_total: f64,
    index_selectivity: f64,
    /// A bitmap path's index scan cost, without the bitmap's charge.
    bitmap_index_total: f64,
    /// The path returns rows in the query's order.
    sorted: bool,
    /// An index path reads its index backward.
    backward: bool,
    /// A sort above the path orders its rows.
    sort: bool,
}

/// `compare_path_costs_fuzzily` of `new` against `old`: `Some(true)` when
/// `new` is better, `Some(false)` when `old` is, `None` when they are
/// fuzzily equal.
fn fuzzily_better(
    new: &Candidate,
    old: &Candidate,
    fuzz: f64,
    startup: bool,
) -> Option<Option<bool>> {
    if new.disabled != old.disabled {
        return Some(Some(new.disabled < old.disabled));
    }
    if new.total > old.total * fuzz {
        if startup && old.startup > new.startup * fuzz {
            return None;
        }
        return Some(Some(false));
    }
    if old.total > new.total * fuzz {
        if startup && new.startup > old.startup * fuzz {
            return None;
        }
        return Some(Some(true));
    }
    if new.startup > old.startup * fuzz {
        return Some(Some(false));
    }
    if old.startup > new.startup * fuzz {
        return Some(Some(true));
    }
    Some(None)
}

/// `add_path` over paths of one relation with the same rows and
/// parameterization, each with or without the query's ordering: the kept
/// paths, cheapest total first.
fn add_paths(
    paths: &[Option<Candidate>],
    consider_startup: bool,
    kept: &mut [Option<Candidate>; MAX_PATHS],
) -> usize {
    let mut count = 0;
    for new in paths.iter().flatten() {
        let mut accept = true;
        let mut insert_at = 0;
        let mut position = 0;
        while position < count {
            let old = kept[position].expect("kept paths are dense");
            // `compare_pathkeys`: Some(true) when only `new` is ordered.
            let keys = (new.sorted != old.sorted).then_some(new.sorted);
            let mut remove = false;
            match fuzzily_better(new, &old, FUZZ_FACTOR, consider_startup) {
                None => {}
                Some(Some(true)) => remove = keys != Some(false),
                Some(Some(false)) => accept = keys == Some(true),
                Some(None) => match keys {
                    Some(true) => remove = true,
                    Some(false) => accept = false,
                    None => match fuzzily_better(new, &old, 1.000_000_000_1, consider_startup) {
                        Some(Some(true)) => remove = true,
                        _ => accept = false,
                    },
                },
            }
            if remove {
                for at in position..count - 1 {
                    kept[at] = kept[at + 1];
                }
                count -= 1;
                kept[count] = None;
                continue;
            }
            if new.disabled > old.disabled
                || (new.disabled == old.disabled && new.total >= old.total)
            {
                insert_at = position + 1;
            }
            if !accept {
                break;
            }
            position += 1;
        }
        if accept {
            for at in (insert_at..count).rev() {
                kept[at + 1] = kept[at];
            }
            kept[insert_at] = Some(*new);
            count += 1;
        }
    }
    count
}

/// `set_cheapest`: the lowest total cost, then startup cost; the first on a
/// tie.
fn cheapest_total(paths: &[Option<Candidate>]) -> Option<Candidate> {
    paths.iter().flatten().copied().reduce(|best, path| {
        let order = path
            .disabled
            .cmp(&best.disabled)
            .then(path.total.total_cmp(&best.total))
            .then(path.startup.total_cmp(&best.startup));
        if order.is_lt() { path } else { best }
    })
}

/// Analyzes `conjunct` as a restriction of table 0 of `scope`.
fn restriction<'a>(
    scope: &QueryScope<'_>,
    conjunct: &'a Expr<'a>,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Option<Shape<'a>>, SqlError> {
    let column_of = |expression: &Expr<'_>| -> Option<usize> {
        let Expr::Column { qualifier, name } = expression else {
            return None;
        };
        match scope.find_column(*qualifier, name).ok()? {
            ResolvedColumn::Table(0, column) => Some(column),
            _ => None,
        }
    };
    let constant =
        |expression: &'a Expr<'a>, column: usize| -> Result<Option<Datum<'a>>, SqlError> {
            if !expression.is_constant(false) || expression.calls_routine() {
                return Ok(None);
            }
            let Ok(value) =
                crate::sql::eval::eval(expression, arena, params, &crate::sql::eval::NoColumns)
            else {
                return Ok(None);
            };
            // An untyped literal takes the column's type, as the parser
            // resolves it.
            if matches!(expression, Expr::Str(_)) && !value.is_null() {
                let ctype = scope
                    .defs
                    .stored(0)
                    .and_then(|definition| definition.columns().get(column))
                    .map(|meta| meta.ctype);
                // Only types whose literal input has no side effect (a notice)
                // are read here.
                return Ok(match ctype {
                    Some(ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name)
                    | None => Some(value),
                    Some(
                        ctype @ (ColType::Bool
                        | ColType::Int2
                        | ColType::Int4
                        | ColType::Int8
                        | ColType::Float4
                        | ColType::Float8
                        | ColType::Numeric
                        | ColType::Oid
                        | ColType::Date
                        | ColType::Time
                        | ColType::Timetz
                        | ColType::Timestamp
                        | ColType::Timestamptz
                        | ColType::Interval
                        | ColType::Bytea
                        | ColType::Uuid
                        | ColType::Char),
                    ) => crate::sql::eval::cast_to(value, ctype, arena).ok(),
                    Some(_) => None,
                });
            }
            Ok(Some(value))
        };
    Ok(match conjunct {
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => {
            let (column, key, operand, on_left) = match (column_of(left), column_of(right)) {
                (Some(column), None) => (column, *left, *right, true),
                (None, Some(column)) => (column, *right, *left, false),
                _ => return Ok(None),
            };
            let Some(comparison) = Comparison::of(*operator, on_left) else {
                return Ok(None);
            };
            // A generic plan's parameter is a constant of unknown value.
            let value = if matches!(operand, Expr::Param(_)) && params.is_empty() {
                None
            } else {
                let Some(value) = constant(operand, column)? else {
                    return Ok(None);
                };
                Some(value)
            };
            Some(Shape::Compare {
                column,
                comparison,
                constant: value,
                key,
                operand,
            })
        }
        Expr::IsNull { operand, negated } => column_of(operand).map(|column| Shape::NullTest {
            column,
            is_null: !negated,
            key: operand,
        }),
        _ => None,
    })
}

/// The path PostgreSQL chooses to scan table 0 of the one-relation `scope`
/// under `condition`, reading the columns `needed` (`None` when the whole
/// row is read); `None` when the table or the condition is outside the
/// model.
#[expect(clippy::too_many_arguments, reason = "planner inputs are explicit")]
pub(crate) fn scan_path<'a>(
    storage: &Storage,
    scope: &QueryScope<'_>,
    txid: u32,
    condition: Option<&'a Expr<'a>>,
    needed: Option<u64>,
    params: &[Datum<'a>],
    arena: &'a Arena,
    shape: QueryShape,
) -> Result<Option<ScanPath<'a>>, SqlError> {
    let methods = crate::sql::guc::planner_methods();
    if scope.n != 1 || scope.derived[0].is_some() || scope.lateral[0] {
        return Ok(None);
    }
    let slot = scope.slots[0];
    if slot == usize::MAX || storage.relation_has_descendants(slot, txid) {
        return Ok(None);
    }
    let Some(definition) = scope.defs.stored(0) else {
        return Ok(None);
    };
    if definition.kind != crate::storage::TableKind::Local
        || definition.partition.is_partitioned()
        || definition.access_method != crate::storage::TableAccessMethod::Heap
    {
        return Ok(None);
    }
    // Extended statistics change PostgreSQL's selectivities in ways the
    // model leaves out.
    if storage
        .extended_statistics_for_table(slot, txid)
        .next()
        .is_some()
    {
        return Ok(None);
    }
    // The table's indexes, newest first as `get_relation_info` lists them.
    let mut indexes = [None::<PlannerIndex>; MAX_INDEXES];
    let mut index_count = 0;
    let mut outside = false;
    crate::sql::catalog::planner_indexes(storage, txid, slot, &mut |index| {
        if index_count == MAX_INDEXES
            || index.method != IndexAccessMethod::Btree
            || !index.plain
            || index.n_cols != 1
        {
            outside = true;
            return;
        }
        indexes[index_count] = Some(index);
        index_count += 1;
    });
    if outside {
        return Ok(None);
    }
    let indexes = &mut indexes[..index_count];
    indexes.sort_unstable_by_key(|index| core::cmp::Reverse(index.map_or(0, |index| index.oid)));
    let indexes = arena
        .alloc_slice_with(index_count, |position| {
            indexes[position].expect("dense indexes")
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    let statistics = storage.table_statistics(slot, txid);
    let planner = Planner {
        storage,
        txid,
        slot,
        definition,
        statistics: &statistics,
        size: relation_size(storage, slot, txid),
        costs: Costs::current(),
        indexes,
        arena,
    };
    // The restrictions, each analyzed.
    let mut conjuncts = [&Expr::Null; MAX_RESTRICTIONS];
    let mut count = 0;
    if let Some(condition) = condition
        && !super::plan::flatten_and(condition, &mut conjuncts, &mut count)
    {
        return Ok(None);
    }
    let mut restrictions = [None::<Restriction<'a>>; MAX_RESTRICTIONS];
    for (position, &conjunct) in conjuncts[..count].iter().enumerate() {
        let Some(shape) = restriction(scope, conjunct, params, arena)? else {
            return Ok(None);
        };
        let selectivity = match shape {
            Shape::Compare {
                column,
                comparison,
                constant,
                ..
            } => match (comparison, constant) {
                (Comparison::Eq, Some(constant)) => {
                    planner.equality_selectivity(column, constant, false)?
                }
                (Comparison::Ne, Some(constant)) => {
                    planner.equality_selectivity(column, constant, true)?
                }
                (Comparison::Eq, None) => planner.unknown_equality_selectivity(column, false),
                (Comparison::Ne, None) => planner.unknown_equality_selectivity(column, true),
                // `scalarineqsel` needs a constant; without one PostgreSQL
                // assumes a third of the rows.
                (_, None) => DEFAULT_INEQ_SEL,
                (_, Some(constant)) => {
                    match planner.inequality_selectivity(column, comparison, constant)? {
                        Ok(selectivity) => selectivity,
                        Err(()) => return Ok(None),
                    }
                }
            },
            Shape::NullTest {
                column, is_null, ..
            } => planner.null_selectivity(column, is_null),
        };
        let cost = match shape {
            Shape::Compare { .. } => planner.costs.cpu_operator,
            // A null test is no function call.
            Shape::NullTest { .. } => 0.0,
        };
        restrictions[position] = Some(Restriction {
            source: conjunct,
            shape,
            selectivity,
            cost,
        });
    }
    let restrictions = arena
        .alloc_slice_with(count, |position| restrictions[position].expect("analyzed"))
        .map_err(|_| crate::sql::eval::arena_full())?;
    let all_cost: f64 = restrictions
        .iter()
        .map(|restriction| restriction.cost)
        .sum();
    // An ordering column an equality fixes orders nothing
    // (`pathkey_is_redundant`).
    let order = shape.order.filter(|order| {
        !restrictions.iter().any(|restriction| {
            matches!(
                restriction.shape,
                Shape::Compare {
                    column,
                    comparison: Comparison::Eq,
                    ..
                } if column == order.column
            )
        })
    });
    let rows = clamp_row_est(planner.size.tuples * planner.selectivity(restrictions));
    let costs = planner.costs;
    let size = planner.size;
    let mut paths = [None::<Candidate>; MAX_PATHS];
    let mut path_count = 0;
    // `cost_seqscan`.
    paths[path_count] = Some(Candidate {
        kind: ScanKind::Seq,
        index: usize::MAX,
        startup: 0.0,
        total: costs.seq_page * size.pages + (costs.cpu_tuple + all_cost) * size.tuples,
        disabled: u32::from(!methods.seqscan),
        index_total: 0.0,
        index_selectivity: 1.0,
        bitmap_index_total: 0.0,
        sorted: order.is_none(),
        backward: false,
        sort: false,
    });
    path_count += 1;
    // Each index's path, and the bitmap paths of those with conditions.
    let mut bitmap_candidates = [usize::MAX; MAX_INDEXES];
    let mut bitmap_count = 0;
    for (position, index) in planner.indexes.iter().enumerate() {
        let column = usize::from(index.columns[0]);
        let mut quals = [None::<Restriction<'a>>; MAX_RESTRICTIONS];
        let mut qual_count = 0;
        let mut other_cost = 0.0;
        for restriction in restrictions.iter() {
            if restriction.shape.column() == column && restriction.shape.indexable() {
                quals[qual_count] = Some(*restriction);
                qual_count += 1;
            } else {
                other_cost += restriction.cost;
            }
        }
        let filler = restrictions.first().copied();
        let mut quals_array: [Restriction<'a>; MAX_RESTRICTIONS] = match filler {
            Some(filler) => [filler; MAX_RESTRICTIONS],
            None => {
                // No restriction: no index condition either.
                [Restriction {
                    source: &Expr::Null,
                    shape: Shape::NullTest {
                        column: usize::MAX,
                        is_null: true,
                        key: &Expr::Null,
                    },
                    selectivity: 1.0,
                    cost: 0.0,
                }; MAX_RESTRICTIONS]
            }
        };
        for (at, qual) in quals[..qual_count].iter().enumerate() {
            quals_array[at] = qual.expect("dense quals");
        }
        let quals = &quals_array[..qual_count];
        let index_only = methods.indexonlyscan
            && needed.is_some_and(|needed| {
                let mut covered = 1u64 << column.min(63);
                for &include in &index.include_columns[..index.n_include_cols] {
                    covered |= 1u64 << usize::from(include).min(63);
                }
                needed & !covered == 0 && column < 64
            });
        // `build_index_pathkeys`: a forward scan returns the key in the
        // index's order, a backward scan in the reverse.
        let forward_sorted = order.is_some_and(|order| {
            order.column == column
                && order.descending == index.descending[0]
                && order.nulls_first == index.nulls_first[0]
        });
        let backward_sorted = order.is_some_and(|order| {
            order.column == column
                && order.descending != index.descending[0]
                && order.nulls_first != index.nulls_first[0]
        });
        if quals.is_empty() && !index_only && !forward_sorted && !backward_sorted {
            continue;
        }
        let selectivity = planner.selectivity(quals);
        let meta = &definition.columns()[column];
        let variable = planner.variable(column);
        let key_width = if meta.ctype.physical_layout().length > 0 {
            f64::from(meta.ctype.physical_layout().length)
        } else {
            definition
                .attributes
                .number(column)
                .and_then(|attribute| statistics.column(attribute))
                .map_or(32.0, |column| f64::from(column.average_width))
        };
        let index_tuples = size.tuples;
        let distinct = if index.unique {
            index_tuples
        } else {
            planner.distinct(&variable).0
        };
        // The index's pages are its actual size: its entries for the rows
        // it holds now, which a table never counted estimates otherwise.
        let entries = if statistics.valid {
            statistics.rows as f64
        } else {
            storage.planning_row_estimate(slot) as f64
        };
        let distinct = distinct.min(entries.max(1.0));
        let btree = btree_size(
            entries,
            key_width,
            distinct,
            !index.unique && deduplicates(meta.ctype, meta.collation),
        );
        // `btcostestimate`.
        let unique_equality = index.unique
            && quals.iter().any(|qual| {
                matches!(
                    qual.shape,
                    Shape::Compare {
                        comparison: Comparison::Eq,
                        ..
                    }
                )
            })
            && !quals
                .iter()
                .any(|qual| matches!(qual.shape, Shape::NullTest { is_null: true, .. }));
        let mut index_rows = if unique_equality {
            1.0
        } else {
            (selectivity * size.tuples).round()
        };
        // `genericcostestimate`.
        if index_rows > index_tuples {
            index_rows = index_tuples;
        }
        if index_rows < 1.0 {
            index_rows = 1.0;
        }
        let index_pages = if btree.pages > 1.0 && index_tuples > 1.0 {
            (index_rows * btree.pages / index_tuples).ceil()
        } else {
            1.0
        };
        let qual_cost = costs.cpu_operator * quals.len() as f64;
        let mut index_startup = 0.0;
        let mut index_total = index_pages * costs.random_page;
        index_total += index_rows * (costs.cpu_index_tuple + qual_cost);
        if index_tuples > 1.0 {
            let descent = (index_tuples.ln() / 2.0f64.ln()).ceil() * costs.cpu_operator;
            index_startup += descent;
            index_total += descent;
        }
        let descent = f64::from(btree.height + 1) * 50.0 * costs.cpu_operator;
        index_startup += descent;
        index_total += descent;
        let correlation = variable
            .distribution
            .and_then(|distribution| distribution.correlation())
            .map_or(0.0, |correlation| {
                let correlation = f64::from(correlation);
                if index.descending[0] {
                    -correlation
                } else {
                    correlation
                }
            });
        // `cost_index`.
        let tuples_fetched = clamp_row_est(selectivity * size.tuples);
        let index_only_fraction = if index_only {
            1.0 - size.allvisfrac
        } else {
            1.0
        };
        let total_pages = (size.pages + btree.pages).max(1.0);
        let pages_fetched = (index_pages_fetched(
            tuples_fetched,
            size.pages,
            total_pages,
            costs.effective_cache_size,
        ) * index_only_fraction)
            .ceil();
        let max_io = pages_fetched * costs.random_page;
        let minimum_pages = ((selectivity * size.pages).ceil() * index_only_fraction).ceil();
        let min_io = if minimum_pages > 0.0 {
            costs.random_page + (minimum_pages - 1.0).max(0.0) * costs.seq_page
        } else {
            0.0
        };
        let run = index_total - index_startup
            + max_io
            + correlation * correlation * (min_io - max_io)
            + (costs.cpu_tuple + other_cost) * tuples_fetched;
        let index_path = Candidate {
            kind: if index_only {
                ScanKind::IndexOnly
            } else {
                ScanKind::Index
            },
            index: position,
            startup: index_startup,
            total: index_startup + run,
            disabled: u32::from(!methods.indexscan),
            index_total,
            index_selectivity: selectivity,
            bitmap_index_total: 0.0,
            // Without a requested order every path's pathkeys are equal.
            sorted: forward_sorted || order.is_none(),
            backward: false,
            sort: false,
        };
        // The forward path, made when it has conditions, an ordering or
        // covers the query; then a backward one that orders.
        if !quals.is_empty() || index_only || forward_sorted {
            paths[path_count] = Some(index_path);
            path_count += 1;
            if !quals.is_empty() && (!forward_sorted || selectivity < 1.0) {
                bitmap_candidates[bitmap_count] = path_count - 1;
                bitmap_count += 1;
            }
        }
        if backward_sorted {
            paths[path_count] = Some(Candidate {
                sorted: true,
                backward: true,
                ..index_path
            });
            path_count += 1;
            if !quals.is_empty() && selectivity < 1.0 {
                bitmap_candidates[bitmap_count] = path_count - 1;
                bitmap_count += 1;
            }
        }
    }
    // `cost_bitmap_heap_scan` of a bitmap whose index tree costs
    // `tree_cost` and keeps `tree_selectivity` of the rows.
    let bitmap_heap = |tree_cost: f64, tree_selectivity: f64| -> (f64, f64) {
        let tuples_fetched = clamp_row_est(tree_selectivity * size.tuples);
        let pages = if size.pages > 1.0 { size.pages } else { 1.0 };
        let mut pages_fetched = (2.0 * pages * tuples_fetched) / (2.0 * pages + tuples_fetched);
        if pages_fetched >= pages {
            pages_fetched = pages;
        } else {
            pages_fetched = pages_fetched.ceil();
        }
        let cost_per_page = if pages_fetched >= 2.0 {
            costs.random_page
                - (costs.random_page - costs.seq_page) * (pages_fetched / pages).sqrt()
        } else {
            costs.random_page
        };
        let run = pages_fetched * cost_per_page + (costs.cpu_tuple + all_cost) * tuples_fetched;
        (tree_cost, tree_cost + run)
    };
    // `cost_bitmap_tree_node` of an index path: its index cost plus a small
    // charge per row for the bitmap.
    let tree_node = |path: &Candidate| {
        (
            path.index_total + 0.1 * costs.cpu_operator * rows,
            path.index_selectivity,
        )
    };
    if bitmap_count > 0 {
        // `choose_bitmap_and`: paths on the same clauses keep the cheaper
        // (single-column indexes on one column use the same clauses), the
        // rest sorted by cost and selectivity, each the leader of an AND
        // group that takes a later path while that lowers the total.
        let mut survivors = [usize::MAX; MAX_INDEXES];
        let mut survivor_count = 0;
        for &candidate in &bitmap_candidates[..bitmap_count] {
            let path = paths[candidate].expect("bitmap candidate");
            let column = usize::from(planner.indexes[path.index].columns[0]);
            let same = survivors[..survivor_count].iter().position(|&survivor| {
                let other = paths[survivor].expect("bitmap candidate");
                usize::from(planner.indexes[other.index].columns[0]) == column
            });
            match same {
                Some(at) => {
                    let other = paths[survivors[at]].expect("bitmap candidate");
                    if tree_node(&path).0 < tree_node(&other).0 {
                        survivors[at] = candidate;
                    }
                }
                None => {
                    survivors[survivor_count] = candidate;
                    survivor_count += 1;
                }
            }
        }
        let survivors = &mut survivors[..survivor_count];
        survivors.sort_unstable_by(|&left, &right| {
            let (left_cost, left_selectivity) = tree_node(&paths[left].expect("survivor"));
            let (right_cost, right_selectivity) = tree_node(&paths[right].expect("survivor"));
            left_cost
                .total_cmp(&right_cost)
                .then(left_selectivity.total_cmp(&right_selectivity))
                .then(left.cmp(&right))
        });
        let mut best: Option<(f64, usize, usize)> = None;
        for leader in 0..survivor_count {
            let (leader_cost, leader_selectivity) =
                tree_node(&paths[survivors[leader]].expect("survivor"));
            let mut group = 1;
            let mut group_cost = leader_cost;
            let mut group_selectivity = leader_selectivity;
            let mut so_far = bitmap_heap(leader_cost, leader_selectivity).1;
            for &next in &survivors[leader + 1..] {
                let (cost, selectivity) = tree_node(&paths[next].expect("survivor"));
                // `cost_bitmap_and_node`.
                let and_cost = group_cost + cost + 100.0 * costs.cpu_operator;
                let and_selectivity = group_selectivity * selectivity;
                let total = bitmap_heap(and_cost, and_selectivity).1;
                if total < so_far {
                    so_far = total;
                    group += 1;
                    group_cost = and_cost;
                    group_selectivity = and_selectivity;
                }
            }
            if best.is_none_or(|(cost, _, _)| so_far < cost) {
                best = Some((so_far, leader, group));
            }
        }
        let (_, leader, group) = best.expect("a bitmap path survives");
        // A `BitmapAnd` of several indexes is outside the model.
        if group > 1 {
            return Ok(None);
        }
        let path = paths[survivors[leader]].expect("survivor");
        let (tree_cost, tree_selectivity) = tree_node(&path);
        let (startup, total) = bitmap_heap(tree_cost, tree_selectivity);
        paths[path_count] = Some(Candidate {
            kind: ScanKind::Bitmap,
            index: path.index,
            startup,
            total,
            disabled: u32::from(!methods.bitmapscan),
            index_total: tree_cost,
            index_selectivity: tree_selectivity,
            bitmap_index_total: path.index_total,
            sorted: order.is_none(),
            backward: false,
            sort: false,
        });
        path_count += 1;
    }
    let candidates = &paths[..path_count];
    let consider_startup = shape
        .limit
        .is_some_and(|(count, _)| count.is_some_and(|count| count > 0.0));
    let mut kept = [None::<Candidate>; MAX_PATHS];
    let kept_count = add_paths(candidates, consider_startup, &mut kept);
    let Some(cheapest) = cheapest_total(&kept[..kept_count]) else {
        return Ok(None);
    };
    // `create_ordered_paths`: each ordered path as it is, the cheapest
    // under a sort; then each under the `LIMIT`.
    let mut finals = [None::<Candidate>; MAX_PATHS];
    let mut final_count = 0;
    for path in kept[..kept_count].iter().flatten() {
        let mut path = *path;
        if !path.sorted {
            if (path.kind, path.index, path.backward)
                != (cheapest.kind, cheapest.index, cheapest.backward)
            {
                continue;
            }
            let Some((startup, total)) = sort_cost(
                path.total,
                rows,
                f64::from(output_width(definition, &statistics, needed)),
                shape.limit,
                costs,
            ) else {
                return Ok(None);
            };
            path = Candidate {
                startup,
                total,
                sorted: true,
                sort: true,
                disabled: path.disabled + u32::from(!methods.sort),
                ..path
            };
        }
        finals[final_count] = Some(path);
        final_count += 1;
    }
    let mut ordered = [None::<Candidate>; MAX_PATHS];
    let ordered_count = add_paths(&finals[..final_count], consider_startup, &mut ordered);
    // `create_limit_path` scales each path's run cost to the rows read.
    if let Some((count, offset)) = shape.limit {
        for path in ordered[..ordered_count].iter_mut().flatten() {
            let mut rows_left = rows;
            if offset > 0.0 {
                let offset_rows = clamp_row_est(offset).min(rows_left);
                path.startup += (path.total - path.startup) * offset_rows / rows;
                rows_left = (rows_left - offset_rows).max(1.0);
            }
            if let Some(count) = count {
                let count_rows = clamp_row_est(count).min(rows_left);
                path.total = path.startup + (path.total - path.startup) * count_rows / rows;
            }
        }
    }
    let Some(chosen) = cheapest_total(&ordered[..ordered_count]) else {
        return Ok(None);
    };
    // The scan's own costs, under any sort and limit.
    let scan = kept[..kept_count]
        .iter()
        .flatten()
        .find(|path| {
            (path.kind, path.index, path.backward) == (chosen.kind, chosen.index, chosen.backward)
        })
        .copied()
        .unwrap_or(chosen);
    let mut path = ScanPath {
        kind: chosen.kind,
        index_oid: 0,
        index_name: crate::util::StackStr::new(),
        index_column: usize::MAX,
        descending: false,
        nulls_first: false,
        conditions: [None; MAX_RESTRICTIONS],
        condition_count: 0,
        rows,
        width: output_width(definition, &statistics, needed),
        startup_cost: scan.startup,
        total_cost: scan.total,
        index_cost: chosen.bitmap_index_total,
        index_rows: clamp_row_est(chosen.index_selectivity * size.tuples),
        backward: chosen.backward,
        sort: chosen.sort,
        deduplicated: false,
        single_row: false,
        // A scan that returns the requested order stops when its limit has
        // the rows it needs.
        stop_after: match (order, shape.limit) {
            (Some(_), Some((Some(count), offset))) if !chosen.sort && shape.stop_at_limit => {
                Some((count.max(0.0) + offset.max(0.0)) as u64)
            }
            _ => None,
        },
    };
    if chosen.kind != ScanKind::Seq {
        let index = planner.indexes[chosen.index];
        let column = usize::from(index.columns[0]);
        let meta = &definition.columns()[column];
        path.deduplicated = !index.unique && deduplicates(meta.ctype, meta.collation);
        path.single_row = index.unique
            && restrictions.iter().any(|restriction| {
                matches!(
                    restriction.shape,
                    Shape::Compare {
                        column: equal,
                        comparison: Comparison::Eq,
                        ..
                    } if equal == column
                )
            });
        path.index_oid = index.oid;
        path.index_name = index.name;
        path.index_column = column;
        path.descending = index.descending[0];
        path.nulls_first = index.nulls_first[0];
        for restriction in restrictions.iter() {
            if restriction.shape.column() != column || !restriction.shape.indexable() {
                continue;
            }
            path.conditions[path.condition_count] = Some(match restriction.shape {
                Shape::Compare {
                    comparison,
                    key,
                    operand,
                    ..
                } => IndexCondition {
                    source: restriction.source,
                    key,
                    operator: Some(comparison.binary()),
                    operand: Some(operand),
                },
                Shape::NullTest { key, .. } => IndexCondition {
                    source: restriction.source,
                    key,
                    operator: None,
                    operand: None,
                },
            });
            path.condition_count += 1;
        }
    }
    let _ = (planner.slot, MAX_COLUMNS);
    Ok(Some(path))
}

/// The width of the columns the scan returns (`set_rel_width`): each
/// column's analyzed width, or its type's.
fn output_width(definition: &TableDef, statistics: &TableStatistics, needed: Option<u64>) -> u32 {
    let mut width = 0.0;
    for (column, meta) in definition.columns().iter().enumerate() {
        if needed.is_some_and(|needed| column >= 64 || needed & (1 << column) == 0) {
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
fn index_pages_fetched(
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

/// The shape of a query over one relation that its scan path depends on:
/// `None` when its ordering is more than one column of the relation, or its
/// `LIMIT` or `OFFSET` is not a constant.
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
) -> Result<Option<QueryShape>, SqlError> {
    let order = match order_by {
        [] => None,
        [order] => {
            let target =
                super::resolve_order_target(order.expression, items, scope, arena, storage, txid)?;
            let target = match target {
                Expr::Collate { .. } => return Ok(None),
                target => target,
            };
            let Expr::Column { qualifier, name } = target else {
                return Ok(None);
            };
            let Ok(ResolvedColumn::Table(0, column)) = scope.find_column(*qualifier, name) else {
                return Ok(None);
            };
            Some(RequestedOrder {
                column,
                descending: order.descending,
                nulls_first: order.nulls_first,
            })
        }
        _ => return Ok(None),
    };
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
    let (Some(count), Some(skip)) = (constant(limit)?, constant(offset)?) else {
        return Ok(None);
    };
    Ok(Some(QueryShape {
        order,
        limit: (limit.is_some() || offset.is_some()).then_some((count, skip.unwrap_or(0.0))),
        stop_at_limit: false,
    }))
}

/// The shape and read columns of the query whose scan of one relation is
/// running, which that scan's path depends on.
#[derive(Clone, Copy)]
struct ScanContext {
    slot: usize,
    needed: Option<u64>,
    shape: QueryShape,
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
    needed: Option<u64>,
    shape: Option<QueryShape>,
) -> ScanContextScope {
    let _ = condition;
    let context = shape.map(|shape| ScanContext {
        slot,
        needed,
        shape,
    });
    ScanContextScope(SCAN_CONTEXT.with(|current| current.replace(context)))
}

/// The shape and read columns of the running query when its scan of table
/// `slot` under `condition` is this one: the scan path depends on them.
pub(crate) fn running_query(
    slot: usize,
    condition: Option<&Expr<'_>>,
) -> Option<(QueryShape, Option<u64>)> {
    let context = SCAN_CONTEXT.with(core::cell::Cell::get)?;
    let _ = condition;
    (context.slot == slot).then_some((context.shape, context.needed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(kind: ScanKind, startup: f64, total: f64, sorted: bool) -> Candidate {
        Candidate {
            kind,
            index: 0,
            startup,
            total,
            disabled: 0,
            index_total: 0.0,
            index_selectivity: 1.0,
            bitmap_index_total: 0.0,
            sorted,
            backward: false,
            sort: false,
        }
    }

    #[test]
    fn add_path_keeps_the_first_of_fuzzily_equal_paths_and_ordered_ones() {
        let mut kept = [None; MAX_PATHS];
        // Within one percent the earlier path stays.
        let count = add_paths(
            &[
                Some(path(ScanKind::Seq, 0.0, 100.0, false)),
                Some(path(ScanKind::Bitmap, 0.0, 100.0, false)),
            ],
            false,
            &mut kept,
        );
        assert_eq!(count, 1);
        assert_eq!(cheapest_total(&kept[..count]).unwrap().kind, ScanKind::Seq);
        // Fuzzily equal but strictly cheaper replaces the earlier path.
        let count = add_paths(
            &[
                Some(path(ScanKind::Seq, 0.0, 100.0, false)),
                Some(path(ScanKind::Bitmap, 0.0, 99.5, false)),
            ],
            false,
            &mut kept,
        );
        assert_eq!(count, 1);
        assert_eq!(
            cheapest_total(&kept[..count]).unwrap().kind,
            ScanKind::Bitmap
        );
        // A dearer path that returns the requested order survives.
        let count = add_paths(
            &[
                Some(path(ScanKind::Seq, 0.0, 100.0, false)),
                Some(path(ScanKind::Index, 0.3, 150.0, true)),
            ],
            false,
            &mut kept,
        );
        assert_eq!(count, 2);
        // Disabled paths lose to enabled ones whatever their cost.
        let mut disabled = path(ScanKind::Seq, 0.0, 1.0, false);
        disabled.disabled = 1;
        let count = add_paths(
            &[
                Some(disabled),
                Some(path(ScanKind::Index, 0.0, 50.0, false)),
            ],
            false,
            &mut kept,
        );
        assert_eq!(
            cheapest_total(&kept[..count]).unwrap().kind,
            ScanKind::Index
        );
    }

    #[test]
    fn btree_layout_follows_create_index() {
        // 2,000 unique int4 keys: a metapage, a root and six leaves.
        let unique = btree_size(2000.0, 4.0, 2000.0, false);
        assert_eq!((unique.pages, unique.height), (8.0, 1));
        // 40 duplicates of 50 keys deduplicate into posting lists.
        let duplicated = btree_size(2000.0, 4.0, 50.0, true);
        assert_eq!((duplicated.pages, duplicated.height), (4.0, 1));
        // A small index is its root leaf.
        assert_eq!(btree_size(50.0, 4.0, 50.0, false).height, 0);
    }

    #[test]
    fn strings_scale_past_their_common_prefix() {
        let (value, low, high) = string_scalars(b"k5", b"k1", b"k9");
        assert!(low < value && value < high);
        assert_eq!(clamp_row_est(0.4), 1.0);
        assert_eq!(clamp_row_est(39.6), 40.0);
    }
}
