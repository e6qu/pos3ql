//! PostgreSQL 18's planner settings: every parameter `pg_settings` files
//! under "Query Tuning". Each is accepted, validated, shown and reported by
//! `EXPLAIN (SETTINGS)` as PostgreSQL does. The method switches pos3ql's
//! planner has a choice for steer it through [`PlannerMethods`]; the others
//! name plan types or cost inputs pos3ql's planner does not have, so they
//! change no plan, as disabling an unused method changes none in PostgreSQL.
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/misc/guc_tables.c

use core::fmt::Write as _;

use super::{IntSetting, SettingUnit};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;

#[derive(Clone, Copy)]
pub(super) enum PlannerKind {
    Bool(bool),
    Int(IntSetting, i32),
    Real { default: f64, min: f64, max: f64 },
    Enum(u8, &'static [&'static str]),
}

#[derive(Clone, Copy)]
pub(super) struct PlannerSettingDef {
    pub(super) name: &'static str,
    pub(super) kind: PlannerKind,
    pub(super) category: &'static str,
    pub(super) description: &'static str,
}

const METHOD: &str = "Query Tuning / Planner Method Configuration";
const COST: &str = "Query Tuning / Planner Cost Constants";
const GEQO: &str = "Query Tuning / Genetic Query Optimizer";
const OTHER: &str = "Query Tuning / Other Planner Options";

const fn method(name: &'static str, default: bool, description: &'static str) -> PlannerSettingDef {
    PlannerSettingDef {
        name,
        kind: PlannerKind::Bool(default),
        category: METHOD,
        description,
    }
}

const fn real(
    name: &'static str,
    default: f64,
    min: f64,
    max: f64,
    category: &'static str,
    description: &'static str,
) -> PlannerSettingDef {
    PlannerSettingDef {
        name,
        kind: PlannerKind::Real { default, min, max },
        category,
        description,
    }
}

const fn int(
    name: &'static str,
    unit: SettingUnit,
    default: i32,
    min: i32,
    max: i32,
    category: &'static str,
    description: &'static str,
) -> PlannerSettingDef {
    PlannerSettingDef {
        name,
        kind: PlannerKind::Int(
            IntSetting {
                name,
                unit,
                min,
                max,
            },
            default,
        ),
        category,
        description,
    }
}

pub(super) const PLANNER_SETTING_COUNT: usize = 52;

/// Ordered by name, as `SHOW ALL` lists them.
pub(super) const PLANNER_SETTINGS: [PlannerSettingDef; PLANNER_SETTING_COUNT] = [
    PlannerSettingDef {
        name: "constraint_exclusion",
        kind: PlannerKind::Enum(0, &["partition", "on", "off"]),
        category: OTHER,
        description: "Enables the planner to use constraints to optimize queries.",
    },
    real(
        "cpu_index_tuple_cost",
        0.005,
        0.0,
        f64::MAX,
        COST,
        "Sets the planner's estimate of the cost of processing each index entry during an index scan.",
    ),
    real(
        "cpu_operator_cost",
        0.0025,
        0.0,
        f64::MAX,
        COST,
        "Sets the planner's estimate of the cost of processing each operator or function call.",
    ),
    real(
        "cpu_tuple_cost",
        0.01,
        0.0,
        f64::MAX,
        COST,
        "Sets the planner's estimate of the cost of processing each tuple (row).",
    ),
    real(
        "cursor_tuple_fraction",
        0.1,
        0.0,
        1.0,
        OTHER,
        "Sets the planner's estimate of the fraction of a cursor's rows that will be retrieved.",
    ),
    int(
        "default_statistics_target",
        SettingUnit::None,
        100,
        1,
        10_000,
        OTHER,
        "Sets the default statistics target.",
    ),
    int(
        "effective_cache_size",
        SettingUnit::Blocks,
        524_288,
        1,
        i32::MAX,
        COST,
        "Sets the planner's assumption about the total size of the data caches.",
    ),
    method(
        "enable_async_append",
        true,
        "Enables the planner's use of async append plans.",
    ),
    method(
        "enable_bitmapscan",
        true,
        "Enables the planner's use of bitmap-scan plans.",
    ),
    method(
        "enable_distinct_reordering",
        true,
        "Enables reordering of DISTINCT keys.",
    ),
    method(
        "enable_gathermerge",
        true,
        "Enables the planner's use of gather merge plans.",
    ),
    method(
        "enable_group_by_reordering",
        true,
        "Enables reordering of GROUP BY keys.",
    ),
    method(
        "enable_hashagg",
        true,
        "Enables the planner's use of hashed aggregation plans.",
    ),
    method(
        "enable_hashjoin",
        true,
        "Enables the planner's use of hash join plans.",
    ),
    method(
        "enable_incremental_sort",
        true,
        "Enables the planner's use of incremental sort steps.",
    ),
    method(
        "enable_indexonlyscan",
        true,
        "Enables the planner's use of index-only-scan plans.",
    ),
    method(
        "enable_indexscan",
        true,
        "Enables the planner's use of index-scan plans.",
    ),
    method(
        "enable_material",
        true,
        "Enables the planner's use of materialization.",
    ),
    method(
        "enable_memoize",
        true,
        "Enables the planner's use of memoization.",
    ),
    method(
        "enable_mergejoin",
        true,
        "Enables the planner's use of merge join plans.",
    ),
    method(
        "enable_nestloop",
        true,
        "Enables the planner's use of nested-loop join plans.",
    ),
    method(
        "enable_parallel_append",
        true,
        "Enables the planner's use of parallel append plans.",
    ),
    method(
        "enable_parallel_hash",
        true,
        "Enables the planner's use of parallel hash plans.",
    ),
    method(
        "enable_partition_pruning",
        true,
        "Enables plan-time and execution-time partition pruning.",
    ),
    method(
        "enable_partitionwise_aggregate",
        false,
        "Enables partitionwise aggregation and grouping.",
    ),
    method(
        "enable_partitionwise_join",
        false,
        "Enables partitionwise join.",
    ),
    method(
        "enable_presorted_aggregate",
        true,
        "Enables the planner's ability to produce plans that provide presorted input for ORDER BY / DISTINCT aggregate functions.",
    ),
    method(
        "enable_self_join_elimination",
        true,
        "Enables removal of unique self-joins.",
    ),
    method(
        "enable_seqscan",
        true,
        "Enables the planner's use of sequential-scan plans.",
    ),
    method(
        "enable_sort",
        true,
        "Enables the planner's use of explicit sort steps.",
    ),
    method(
        "enable_tidscan",
        true,
        "Enables the planner's use of TID scan plans.",
    ),
    int(
        "from_collapse_limit",
        SettingUnit::None,
        8,
        1,
        i32::MAX,
        OTHER,
        "Sets the FROM-list size beyond which subqueries are not collapsed.",
    ),
    PlannerSettingDef {
        name: "geqo",
        kind: PlannerKind::Bool(true),
        category: GEQO,
        description: "Enables genetic query optimization.",
    },
    int(
        "geqo_effort",
        SettingUnit::None,
        5,
        1,
        10,
        GEQO,
        "GEQO: effort is used to set the default for other GEQO parameters.",
    ),
    int(
        "geqo_generations",
        SettingUnit::None,
        0,
        0,
        i32::MAX,
        GEQO,
        "GEQO: number of iterations of the algorithm.",
    ),
    int(
        "geqo_pool_size",
        SettingUnit::None,
        0,
        0,
        i32::MAX,
        GEQO,
        "GEQO: number of individuals in the population.",
    ),
    real(
        "geqo_seed",
        0.0,
        0.0,
        1.0,
        GEQO,
        "GEQO: seed for random path selection.",
    ),
    real(
        "geqo_selection_bias",
        2.0,
        1.5,
        2.0,
        GEQO,
        "GEQO: selective pressure within the population.",
    ),
    int(
        "geqo_threshold",
        SettingUnit::None,
        12,
        2,
        i32::MAX,
        GEQO,
        "Sets the threshold of FROM items beyond which GEQO is used.",
    ),
    PlannerSettingDef {
        name: "jit",
        kind: PlannerKind::Bool(true),
        category: OTHER,
        description: "Allow JIT compilation.",
    },
    real(
        "jit_above_cost",
        100_000.0,
        -1.0,
        f64::MAX,
        COST,
        "Perform JIT compilation if query is more expensive.",
    ),
    real(
        "jit_inline_above_cost",
        500_000.0,
        -1.0,
        f64::MAX,
        COST,
        "Perform JIT inlining if query is more expensive.",
    ),
    real(
        "jit_optimize_above_cost",
        500_000.0,
        -1.0,
        f64::MAX,
        COST,
        "Optimize JIT-compiled functions if query is more expensive.",
    ),
    int(
        "join_collapse_limit",
        SettingUnit::None,
        8,
        1,
        i32::MAX,
        OTHER,
        "Sets the FROM-list size beyond which JOIN constructs are not flattened.",
    ),
    int(
        "min_parallel_index_scan_size",
        SettingUnit::Blocks,
        64,
        0,
        i32::MAX / 3,
        COST,
        "Sets the minimum amount of index data for a parallel scan.",
    ),
    int(
        "min_parallel_table_scan_size",
        SettingUnit::Blocks,
        1024,
        0,
        i32::MAX / 3,
        COST,
        "Sets the minimum amount of table data for a parallel scan.",
    ),
    real(
        "parallel_setup_cost",
        1000.0,
        0.0,
        f64::MAX,
        COST,
        "Sets the planner's estimate of the cost of starting up worker processes for parallel query.",
    ),
    real(
        "parallel_tuple_cost",
        0.1,
        0.0,
        f64::MAX,
        COST,
        "Sets the planner's estimate of the cost of passing each tuple (row) from worker to leader backend.",
    ),
    PlannerSettingDef {
        name: "plan_cache_mode",
        kind: PlannerKind::Enum(0, &["auto", "force_generic_plan", "force_custom_plan"]),
        category: OTHER,
        description: "Controls the planner's selection of custom or generic plan.",
    },
    real(
        "random_page_cost",
        4.0,
        0.0,
        f64::MAX,
        COST,
        "Sets the planner's estimate of the cost of a nonsequentially fetched disk page.",
    ),
    real(
        "recursive_worktable_factor",
        10.0,
        0.001,
        1_000_000.0,
        OTHER,
        "Sets the planner's estimate of the average size of a recursive query's working table.",
    ),
    real(
        "seq_page_cost",
        1.0,
        0.0,
        f64::MAX,
        COST,
        "Sets the planner's estimate of the cost of a sequentially fetched disk page.",
    ),
];

/// One stored value per planner setting: booleans as 0 or 1, enums as the
/// index of their name, integers and reals as themselves.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct PlannerValues(pub(super) [f64; PLANNER_SETTING_COUNT]);

impl PlannerValues {
    pub(super) const DEFAULTS: Self = {
        let mut values = [0.0; PLANNER_SETTING_COUNT];
        let mut index = 0;
        while index < PLANNER_SETTING_COUNT {
            values[index] = PLANNER_SETTINGS[index].kind.default_value();
            index += 1;
        }
        Self(values)
    };

    fn flag(&self, name: &str) -> bool {
        self.0[planner_index(name).expect("planner method switch")] != 0.0
    }

    pub(super) fn methods(&self) -> PlannerMethods {
        PlannerMethods {
            seqscan: self.flag("enable_seqscan"),
            indexscan: self.flag("enable_indexscan"),
            indexonlyscan: self.flag("enable_indexonlyscan"),
            bitmapscan: self.flag("enable_bitmapscan"),
            hashjoin: self.flag("enable_hashjoin"),
            nestloop: self.flag("enable_nestloop"),
            material: self.flag("enable_material"),
            sort: self.flag("enable_sort"),
            hashagg: self.flag("enable_hashagg"),
        }
    }
}

impl PlannerKind {
    const fn default_value(self) -> f64 {
        match self {
            Self::Bool(default) => {
                if default {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Int(_, default) => default as f64,
            Self::Real { default, .. } => default,
            Self::Enum(default, _) => default as f64,
        }
    }

    /// The `pg_settings.vartype` of this kind.
    pub(super) fn vartype(self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Int(..) => "integer",
            Self::Real { .. } => "real",
            Self::Enum(..) => "enum",
        }
    }
}

/// The plan methods a session allows, for the planner choices pos3ql has.
/// A disabled method is still used when no enabled alternative can run the
/// step, and `EXPLAIN` then marks its node `Disabled: true`, as PostgreSQL's
/// disabled-node counting does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PlannerMethods {
    pub(crate) seqscan: bool,
    pub(crate) indexscan: bool,
    pub(crate) indexonlyscan: bool,
    pub(crate) bitmapscan: bool,
    pub(crate) hashjoin: bool,
    pub(crate) nestloop: bool,
    pub(crate) material: bool,
    pub(crate) sort: bool,
    pub(crate) hashagg: bool,
}

impl PlannerMethods {
    pub(crate) const DEFAULT: Self = Self {
        seqscan: true,
        indexscan: true,
        indexonlyscan: true,
        bitmapscan: true,
        hashjoin: true,
        nestloop: true,
        material: true,
        sort: true,
        hashagg: true,
    };
}

pub(super) fn planner_index(name: &str) -> Option<usize> {
    PLANNER_SETTINGS
        .iter()
        .position(|setting| setting.name.eq_ignore_ascii_case(name))
}

/// Parses `value` for planner setting `index` as PostgreSQL's
/// `parse_and_validate_value` does.
pub(super) fn parse(index: usize, value: &str) -> Result<f64, SqlError> {
    let setting = PLANNER_SETTINGS[index];
    match setting.kind {
        PlannerKind::Bool(_) => Ok(f64::from(u8::from(super::parse_boolean_setting(
            setting.name,
            value,
        )?))),
        PlannerKind::Int(int, _) => Ok(f64::from(super::parse_int_setting(int, value)?)),
        PlannerKind::Enum(_, names) => names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(value))
            .map(|position| position as f64)
            .ok_or_else(|| {
                let mut available = StackStr::<128>::new();
                for (position, name) in names.iter().enumerate() {
                    let _ = write!(available, "{}{name}", if position == 0 { "" } else { ", " });
                }
                super::invalid_enum_value(setting.name, value, available.as_str())
            }),
        PlannerKind::Real { min, max, .. } => {
            // PostgreSQL's `parse_real`: a `strtod` number with only trailing
            // white space after it, neither NaN nor out of double range.
            let start = value
                .bytes()
                .position(|byte| !byte.is_ascii_whitespace())
                .unwrap_or(value.len());
            let number = super::parse_c_double(value, start)
                .filter(|(number, end)| {
                    !number.is_nan() && value[*end..].bytes().all(|byte| byte.is_ascii_whitespace())
                })
                .map(|(number, _)| number)
                .ok_or_else(|| super::invalid_setting_value(setting.name, value))?;
            if !(min..=max).contains(&number) {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "{} is outside the valid range for parameter \"{}\" ({} .. {})",
                    format_g(number).as_str(),
                    setting.name,
                    format_g(min).as_str(),
                    format_g(max).as_str()
                ));
            }
            Ok(number)
        }
    }
}

/// `SHOW`'s text for planner setting `index` holding `value`.
pub(super) fn show(index: usize, value: f64) -> StackStr<32> {
    match PLANNER_SETTINGS[index].kind {
        PlannerKind::Bool(_) => StackStr::from_str(if value != 0.0 { "on" } else { "off" }),
        PlannerKind::Int(int, _) => {
            StackStr::from_str(super::show_int_setting(int, value as i32).as_str())
        }
        PlannerKind::Real { .. } => format_g(value),
        PlannerKind::Enum(_, names) => StackStr::from_str(names[value as usize]),
    }
}

/// C's `%g`: six significant digits, in exponent form when the exponent is
/// below -4 or at least 6, without trailing zeros.
pub(crate) fn format_g(value: f64) -> StackStr<32> {
    let mut shown = StackStr::new();
    if value == 0.0 {
        let _ = write!(
            shown,
            "{}",
            if value.is_sign_negative() { "-0" } else { "0" }
        );
        return shown;
    }
    let mut scientific = StackStr::<32>::new();
    let _ = write!(scientific, "{value:.5e}");
    let (mantissa, exponent) = scientific
        .as_str()
        .split_once('e')
        .expect("Rust exponent format");
    let exponent: i32 = exponent.parse().expect("Rust exponent digits");
    let trim = |digits: &str| -> StackStr<32> {
        let digits = if digits.contains('.') {
            digits.trim_end_matches('0').trim_end_matches('.')
        } else {
            digits
        };
        StackStr::from_str(digits)
    };
    if (-4..6).contains(&exponent) {
        let mut fixed = StackStr::<32>::new();
        let _ = write!(fixed, "{value:.*}", (5 - exponent) as usize);
        return trim(fixed.as_str());
    }
    let _ = write!(
        shown,
        "{}e{}{:02}",
        trim(mantissa).as_str(),
        if exponent < 0 { '-' } else { '+' },
        exponent.unsigned_abs()
    );
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_c_percent_g() {
        for (value, expected) in [
            (0.005, "0.005"),
            (1.0, "1"),
            (1_000_000.0, "1e+06"),
            (f64::MAX, "1.79769e+308"),
            (0.000_001_234_567, "1.23457e-06"),
            (1_234_567.0, "1.23457e+06"),
            (0.001, "0.001"),
            (-1.0, "-1"),
            (100_000.0, "100000"),
            (0.0001, "0.0001"),
        ] {
            assert_eq!(format_g(value).as_str(), expected, "{value}");
        }
    }

    #[test]
    fn table_is_sorted_and_defaults_are_valid() {
        for pair in PLANNER_SETTINGS.windows(2) {
            assert!(pair[0].name < pair[1].name, "{}", pair[1].name);
        }
        for (index, setting) in PLANNER_SETTINGS.iter().enumerate() {
            let shown = show(index, PlannerValues::DEFAULTS.0[index]);
            assert_eq!(
                parse(index, shown.as_str()).ok(),
                Some(PlannerValues::DEFAULTS.0[index]),
                "{}",
                setting.name
            );
        }
    }
}
