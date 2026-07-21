//! Aggregate accumulation: one `AggState` per aggregate call.
//!
//! `AggState` folds a column of values into a single result — the plain
//! aggregates, the statistical family, the ordered-set aggregates, and the
//! collecting ones (`string_agg`, `array_agg`, the json aggregates) — handling
//! `DISTINCT`, an aggregate-level `ORDER BY`, and `FILTER (WHERE ...)` at the
//! one choke point `update`. The window executor reuses it per frame.

use core::fmt::Write as _;
use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, FromClause};
use crate::sql::eval::{compare_datums, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError};
use crate::sql::exec::MAX_PROJ;
use crate::sql::types::Datum;
use crate::sql_err;
use crate::storage::Storage;

use super::{arena_full, scan_source, Chained, QueryScope, MAX_AGGS};

#[allow(clippy::too_many_arguments)]
pub(crate) fn fold_aggregates<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer_arg: Option<&dyn ColumnLookup<'a>>,
) -> Result<&'a mut [Datum<'a>], SqlError> {
    let mut states = [AggState::default(); MAX_AGGS];
    for (i, (_, node)) in agg_nodes.iter().enumerate() {
        states[i].init(node)?;
    }
    scan_source(
        storage,
        scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        hooks,
        outer_arg,
        &mut |row| {
            let chained_row = Chained { inner: row, outer: outer_arg };
            for (i, (_, node)) in agg_nodes.iter().enumerate() {
                states[i].update(node, arena, params, &chained_row, hooks)?;
            }
            Ok(true)
        },
    )?;
    let out = arena
        .alloc_slice_with(agg_nodes.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (i, state) in states[..agg_nodes.len()].iter_mut().enumerate() {
        out[i] = state.finish(arena)?;
    }
    Ok(out)
}

#[derive(Clone, Copy)]
pub(crate) struct AggState<'a> {
    kind: AggKind,
    star: bool,
    count: u64,
    sum_int: i128,
    sum_float: f64,
    sum_numeric: Option<crate::sql::numeric::Numeric<'a>>,
    // Statistical-aggregate accumulators (all in f64). `sum_float` holds Σx (the
    // second argument for the two-argument regression/covariance aggregates).
    sum_sq: f64,  // Σx²
    sum_y: f64,   // Σy (first argument of a two-arg aggregate)
    sum_xy: f64,  // Σxy
    sum_yy: f64,  // Σy²
    // Exact Σx² for the single-argument variance/stddev family over
    // integer/numeric inputs, which return numeric (Σx reuses `sum_numeric`).
    sum_sq_numeric: Option<crate::sql::numeric::Numeric<'a>>,
    arg_kind: ArgKind,
    best: Option<Datum<'a>>,
    bool_acc: Option<bool>,
    // `agg(DISTINCT x)`: non-null argument values are buffered here during the
    // scan (a doubling arena-backed vector), then sorted, deduplicated, and
    // folded in `finish`. Empty for non-distinct aggregates.
    distinct: bool,
    vals: *mut Datum<'a>,
    vals_len: usize,
    vals_cap: usize,
    // string_agg: the delimiter (captured on first input, for the DISTINCT
    // fold) and a doubling arena-backed byte buffer of the joined output.
    sep: Option<&'a str>,
    str_buf: *mut u8,
    str_len: usize,
    str_cap: usize,
    // string_agg(x ORDER BY k): each row's `[value, keys...]` tuple is buffered
    // self-describing-encoded, then sorted by the key columns and concatenated
    // in `finish`. `ordered` is only set for string_agg (ORDER BY cannot change
    // a commutative aggregate's result).
    ordered: bool,
    ord_spec: &'a [crate::sql::ast::OrderBy<'a>],
    ord: *mut &'a [u8],
    ord_len: usize,
    ord_cap: usize,
}

/// The most general numeric class seen among an aggregate's inputs, driving
/// PostgreSQL's result type (sum(int4)->int8, sum(int8)->numeric, avg(int)
/// ->numeric, etc.).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ArgKind {
    None,
    Int4,
    Int8,
    Numeric,
    Float,
}

#[derive(Clone, Copy, PartialEq)]
enum AggKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    BoolAnd,
    BoolOr,
    /// Bitwise aggregates over integer or bit-string inputs, reducing with
    /// `&`/`|`/`^`; the running value (its input type preserved) lives in `best`.
    BitAnd,
    BitOr,
    BitXor,
    StringAgg,
    /// `array_agg(expr [ORDER BY ...])`: buffers every value (NULLs kept),
    /// optionally sorted / DISTINCT, then builds a one-dimensional array.
    ArrayAgg,
    /// `json_agg`/`jsonb_agg(expr [ORDER BY ...])`: buffers values, then
    /// serializes them to a JSON array. `star` distinguishes json vs jsonb.
    JsonAgg { jsonb: bool },
    /// `json_object_agg`/`jsonb_object_agg(key, value)`: buffers `[key, value]`
    /// tuples into a JSON object.
    JsonObjectAgg { jsonb: bool },
    /// Ordered-set aggregates: the aggregated values come from `WITHIN GROUP
    /// (ORDER BY ...)` and are buffered (in `vals`), sorted, then reduced in
    /// `finish`. `sum_float` holds the percentile fraction.
    PercentileCont,
    PercentileDisc,
    Mode,
    /// Single-argument statistical aggregates over `n`, `Σx`, `Σx²`.
    VarPop,
    VarSamp,
    StddevPop,
    StddevSamp,
    /// Two-argument `(Y, X)` statistical aggregates, also accumulating `Σy`,
    /// `Σxy`, `Σy²`. Rows where either argument is NULL are skipped.
    Corr,
    CovarPop,
    CovarSamp,
    RegrSlope,
    RegrIntercept,
    RegrR2,
    RegrCount,
    RegrAvgx,
    RegrAvgy,
    RegrSxx,
    RegrSyy,
    RegrSxy,
}

impl AggKind {
    /// The two-argument statistical aggregates `agg(Y, X)`.
    fn is_two_arg_stat(self) -> bool {
        matches!(
            self,
            AggKind::Corr
                | AggKind::CovarPop
                | AggKind::CovarSamp
                | AggKind::RegrSlope
                | AggKind::RegrIntercept
                | AggKind::RegrR2
                | AggKind::RegrCount
                | AggKind::RegrAvgx
                | AggKind::RegrAvgy
                | AggKind::RegrSxx
                | AggKind::RegrSyy
                | AggKind::RegrSxy
        )
    }

    /// The single-argument statistical aggregates over `n`, `Σx`, `Σx²`.
    fn is_one_arg_stat(self) -> bool {
        matches!(
            self,
            AggKind::VarPop | AggKind::VarSamp | AggKind::StddevPop | AggKind::StddevSamp
        )
    }
}

impl Default for AggState<'_> {
    fn default() -> Self {
        Self {
            kind: AggKind::Count,
            star: false,
            count: 0,
            sum_int: 0,
            sum_float: 0.0,
            sum_numeric: None,
            sum_sq: 0.0,
            sum_y: 0.0,
            sum_xy: 0.0,
            sum_yy: 0.0,
            sum_sq_numeric: None,
            arg_kind: ArgKind::None,
            best: None,
            bool_acc: None,
            distinct: false,
            vals: core::ptr::null_mut(),
            vals_len: 0,
            vals_cap: 0,
            sep: None,
            str_buf: core::ptr::null_mut(),
            str_len: 0,
            str_cap: 0,
            ordered: false,
            ord_spec: &[],
            ord: core::ptr::null_mut(),
            ord_len: 0,
            ord_cap: 0,
        }
    }
}

/// A numeric datum as `f64` for the statistical aggregates (None = NULL or a
/// non-numeric value).
fn agg_f64(d: &Datum) -> Option<f64> {
    match d {
        Datum::Int4(v) => Some(*v as f64),
        Datum::Int8(v) => Some(*v as f64),
        Datum::Float8(v) => Some(*v),
        Datum::Numeric(n) => Some(n.to_f64()),
        _ => None,
    }
}

impl<'a> AggState<'a> {
    pub(crate) fn init(&mut self, node: &'a Expr<'a>) -> Result<(), SqlError> {
        let Expr::Call { name, star, distinct, order_by, args, .. } = node else {
            return Err(sql_err!("42803", "not an aggregate"));
        };
        self.kind = match *name {
            "count" => AggKind::Count,
            "sum" => AggKind::Sum,
            "avg" => AggKind::Avg,
            "min" => AggKind::Min,
            "max" => AggKind::Max,
            "bool_and" | "every" => AggKind::BoolAnd,
            "bool_or" => AggKind::BoolOr,
            "bit_and" => AggKind::BitAnd,
            "bit_or" => AggKind::BitOr,
            "bit_xor" => AggKind::BitXor,
            "string_agg" => AggKind::StringAgg,
            "array_agg" => AggKind::ArrayAgg,
            "json_agg" => AggKind::JsonAgg { jsonb: false },
            "jsonb_agg" => AggKind::JsonAgg { jsonb: true },
            "json_object_agg" => AggKind::JsonObjectAgg { jsonb: false },
            "jsonb_object_agg" => AggKind::JsonObjectAgg { jsonb: true },
            "percentile_cont" => AggKind::PercentileCont,
            "percentile_disc" => AggKind::PercentileDisc,
            "mode" => AggKind::Mode,
            "var_pop" => AggKind::VarPop,
            "var_samp" | "variance" => AggKind::VarSamp,
            "stddev_pop" => AggKind::StddevPop,
            "stddev_samp" | "stddev" => AggKind::StddevSamp,
            "corr" => AggKind::Corr,
            "covar_pop" => AggKind::CovarPop,
            "covar_samp" => AggKind::CovarSamp,
            "regr_slope" => AggKind::RegrSlope,
            "regr_intercept" => AggKind::RegrIntercept,
            "regr_r2" => AggKind::RegrR2,
            "regr_count" => AggKind::RegrCount,
            "regr_avgx" => AggKind::RegrAvgx,
            "regr_avgy" => AggKind::RegrAvgy,
            "regr_sxx" => AggKind::RegrSxx,
            "regr_syy" => AggKind::RegrSyy,
            "regr_sxy" => AggKind::RegrSxy,
            other => {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "function {}() is not an aggregate",
                    other
                ))
            }
        };
        self.star = *star;
        self.distinct = *distinct;
        if *distinct && *star {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "DISTINCT is not implemented for count(*)"
            ));
        }
        // ORDER BY only affects string_agg (other aggregates are commutative,
        // so their result is identical regardless of input order).
        if !order_by.is_empty()
            && matches!(
                self.kind,
                AggKind::StringAgg | AggKind::ArrayAgg | AggKind::JsonAgg { .. }
            )
        {
            if *distinct {
                // With DISTINCT, PostgreSQL permits ORDER BY only on the
                // aggregated expression itself.
                let sorts_by_argument =
                    order_by.len() == 1 && args.first().is_some_and(|a| **a == *order_by[0].expression);
                if !sorts_by_argument {
                    return Err(sql_err!(
                        "42P10",
                        "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list"
                    ));
                }
            }
            self.ordered = true;
            self.ord_spec = order_by;
        }
        Ok(())
    }

    pub(crate) fn update(
        &mut self,
        node: &Expr<'a>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        row: &impl ColumnLookup<'a>,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<(), SqlError> {
        let Expr::Call { args, filter, .. } = node else {
            unreachable!("validated in init");
        };
        // `FILTER (WHERE cond)` excludes rows where the condition is not true.
        if let Some(cond) = filter
            && !matches!(eval_full(cond, arena, params, row, hooks)?, Datum::Bool(true))
        {
            return Ok(());
        }
        if self.star {
            self.count += 1;
            return Ok(());
        }
        if self.kind == AggKind::StringAgg {
            return self.update_string_agg(args, arena, params, row, hooks);
        }
        if matches!(self.kind, AggKind::JsonObjectAgg { .. }) {
            if args.len() != 2 {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "json_object_agg requires exactly two arguments"
                ));
            }
            let key = eval_full(args[0], arena, params, row, hooks)?;
            if key.is_null() {
                return Err(sql_err!("22004", "field name must not be null"));
            }
            let value = eval_full(args[1], arena, params, row, hooks)?;
            let tuple = [key, value];
            let enc = crate::sql::exec::encode_projected_pub(&tuple, arena)?;
            // Reuse the ordered buffer to hold the [key, value] pair.
            self.push_ordered(enc, arena)?;
            self.count += 1;
            return Ok(());
        }
        if matches!(self.kind, AggKind::ArrayAgg | AggKind::JsonAgg { .. }) {
            if args.len() != 1 {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "aggregate requires exactly one argument"
                ));
            }
            // array_agg/json_agg keep NULL elements, unlike string_agg.
            let value = eval_full(args[0], arena, params, row, hooks)?;
            if self.ordered {
                let mut tuple = [Datum::Null; 1 + MAX_PROJ];
                tuple[0] = value;
                for (i, o) in self.ord_spec.iter().enumerate() {
                    tuple[1 + i] = eval_full(o.expression, arena, params, row, hooks)?;
                }
                let enc = crate::sql::exec::encode_projected_pub(&tuple[..1 + self.ord_spec.len()], arena)?;
                if self.distinct && self.ord_len > 0 {
                    let seen = unsafe { core::slice::from_raw_parts(self.ord, self.ord_len) };
                    if seen.contains(&enc) {
                        return Ok(());
                    }
                }
                self.push_ordered(enc, arena)?;
            } else {
                self.push_distinct(value, arena)?;
            }
            self.count += 1;
            return Ok(());
        }
        // Ordered-set aggregates buffer their `WITHIN GROUP (ORDER BY expr)`
        // values (reduced in `finish`); `args[0]` is the percentile fraction.
        if matches!(
            self.kind,
            AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode
        ) {
            let Expr::Call { order_by, .. } = node else {
                unreachable!("validated in init");
            };
            let Some(item) = order_by.first() else {
                return Err(sql_err!("42809", "an ordered-set aggregate requires WITHIN GROUP"));
            };
            if matches!(self.kind, AggKind::PercentileCont | AggKind::PercentileDisc)
                && let Some(fraction) = args.first()
            {
                self.sum_float = match eval_full(fraction, arena, params, row, hooks)? {
                    Datum::Float8(f) => f,
                    Datum::Numeric(n) => n.to_f64(),
                    Datum::Int4(v) => f64::from(v),
                    Datum::Int8(v) => v as f64,
                    _ => return Err(sql_err!("2202E", "percentile value must be numeric")),
                };
            }
            let value = eval_full(item.expression, arena, params, row, hooks)?;
            if value.is_null() {
                return Ok(());
            }
            return self.push_distinct(value, arena);
        }
        // Two-argument statistical aggregates `agg(Y, X)`: skip a row where
        // either argument is NULL, else fold Σx, Σx², Σy, Σxy, Σy².
        if self.kind.is_two_arg_stat() {
            if args.len() != 2 {
                return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "aggregate requires two arguments"));
            }
            let y = eval_full(args[0], arena, params, row, hooks)?;
            let x = eval_full(args[1], arena, params, row, hooks)?;
            let (Some(y), Some(x)) = (agg_f64(&y), agg_f64(&x)) else {
                return Ok(());
            };
            self.count += 1;
            self.sum_float += x;
            self.sum_sq += x * x;
            self.sum_y += y;
            self.sum_xy += x * y;
            self.sum_yy += y * y;
            return Ok(());
        }
        let Some(arg) = args.first() else {
            return Err(sql_err!("42803", "aggregate requires an argument"));
        };
        let v = eval_full(arg, arena, params, row, hooks)?;
        if v.is_null() {
            return Ok(());
        }
        // DISTINCT defers folding until finish, so duplicate values can be
        // dropped after the whole group is seen.
        if self.distinct {
            return self.push_distinct(v, arena);
        }
        self.count += 1;
        self.accumulate(v, arena)
    }

    /// Fold one non-null value into the running aggregate (the type-specific
    /// arithmetic shared by the streaming and DISTINCT paths). Callers bump
    /// `count` themselves.
    fn accumulate(&mut self, v: Datum<'a>, arena: &'a Arena) -> Result<(), SqlError> {
        match self.kind {
            AggKind::Count => {}
            AggKind::Sum | AggKind::Avg => match v {
                Datum::Int4(x) => {
                    self.arg_kind = self.arg_kind.max(ArgKind::Int4);
                    self.sum_int += i128::from(x);
                }
                Datum::Int8(x) => {
                    self.arg_kind = self.arg_kind.max(ArgKind::Int8);
                    self.sum_int += i128::from(x);
                }
                Datum::Numeric(n) => {
                    self.arg_kind = self.arg_kind.max(ArgKind::Numeric);
                    let running = self.sum_numeric.unwrap_or(crate::sql::numeric::Numeric::ZERO);
                    self.sum_numeric = Some(crate::sql::numeric::add(&running, &n, arena)?);
                }
                Datum::Float8(x) => {
                    self.arg_kind = ArgKind::Float;
                    self.sum_float += x;
                }
                other => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "cannot sum {:?}",
                        other
                    ))
                }
            },
            AggKind::Min | AggKind::Max => {
                let replace = match &self.best {
                    None => true,
                    Some(b) => {
                        let ord = compare_datums(&v, b)?;
                        (self.kind == AggKind::Min && ord.is_lt())
                            || (self.kind == AggKind::Max && ord.is_gt())
                    }
                };
                if replace {
                    self.best = Some(v);
                }
            }
            AggKind::BoolAnd | AggKind::BoolOr => {
                let Datum::Bool(x) = v else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "bool_and/bool_or requires boolean arguments"
                    ));
                };
                let acc = self.bool_acc.get_or_insert(matches!(self.kind, AggKind::BoolAnd));
                *acc = if self.kind == AggKind::BoolAnd { *acc && x } else { *acc || x };
            }
            // Bitwise aggregates reduce the running value (kept in `best`) with
            // the incoming one via the corresponding bitwise operator.
            AggKind::BitAnd | AggKind::BitOr | AggKind::BitXor => {
                let operator = match self.kind {
                    AggKind::BitAnd => crate::sql::ast::BinaryOp::BitAnd,
                    AggKind::BitOr => crate::sql::ast::BinaryOp::BitOr,
                    _ => crate::sql::ast::BinaryOp::BitXor,
                };
                self.best = Some(match self.best {
                    None => v,
                    Some(acc) => crate::sql::eval::bit_aggregate(operator, acc, v, arena)?,
                });
            }
            // Only reached through the DISTINCT fold; the streaming path handles
            // string_agg directly (it needs the per-row delimiter).
            AggKind::StringAgg => {
                let Datum::Text(s) = v else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "string_agg requires text arguments"
                    ));
                };
                let sep = self.sep.unwrap_or("");
                self.append_str_elem(sep, s, arena)?;
            }
            // Single-argument statistical aggregates fold Σx and Σx². Over
            // integer/numeric inputs PostgreSQL computes and returns an exact
            // numeric result, so those sums are kept in numeric; float8 inputs
            // yield a float8 result and fold in f64.
            AggKind::VarPop | AggKind::VarSamp | AggKind::StddevPop | AggKind::StddevSamp => {
                use crate::sql::numeric::{self as num, Numeric};
                let as_numeric = match v {
                    Datum::Int4(x) => {
                        self.arg_kind = self.arg_kind.max(ArgKind::Int4);
                        Some(Numeric::from_i64(i64::from(x), arena)?)
                    }
                    Datum::Int8(x) => {
                        self.arg_kind = self.arg_kind.max(ArgKind::Int8);
                        Some(Numeric::from_i64(x, arena)?)
                    }
                    Datum::Numeric(n) => {
                        self.arg_kind = self.arg_kind.max(ArgKind::Numeric);
                        Some(n)
                    }
                    Datum::Float8(x) => {
                        self.arg_kind = ArgKind::Float;
                        self.sum_float += x;
                        self.sum_sq += x * x;
                        None
                    }
                    other => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "statistical aggregate requires a numeric argument, got {:?}",
                            other
                        ))
                    }
                };
                if let Some(x) = as_numeric {
                    let sum_x = self.sum_numeric.unwrap_or(Numeric::ZERO);
                    self.sum_numeric = Some(num::add(&sum_x, &x, arena)?);
                    let sum_x2 = self.sum_sq_numeric.unwrap_or(Numeric::ZERO);
                    self.sum_sq_numeric = Some(num::add(&sum_x2, &num::mul(&x, &x, arena)?, arena)?);
                }
            }
            // Ordered-set, array, json, and two-arg statistical aggregates buffer
            // or fold elsewhere; they never reach `accumulate`.
            AggKind::PercentileCont
            | AggKind::PercentileDisc
            | AggKind::Mode
            | AggKind::ArrayAgg
            | AggKind::JsonAgg { .. }
            | AggKind::JsonObjectAgg { .. }
            | AggKind::Corr
            | AggKind::CovarPop
            | AggKind::CovarSamp
            | AggKind::RegrSlope
            | AggKind::RegrIntercept
            | AggKind::RegrR2
            | AggKind::RegrCount
            | AggKind::RegrAvgx
            | AggKind::RegrAvgy
            | AggKind::RegrSxx
            | AggKind::RegrSyy
            | AggKind::RegrSxy => {}
        }
        Ok(())
    }

    /// string_agg streaming path: evaluate value + delimiter, skip NULL values,
    /// and either buffer the value (DISTINCT, folded later) or append it now.
    fn update_string_agg(
        &mut self,
        args: &[&Expr<'a>],
        arena: &'a Arena,
        params: &[Datum<'a>],
        row: &impl ColumnLookup<'a>,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<(), SqlError> {
        if args.len() != 2 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "string_agg requires exactly two arguments"
            ));
        }
        let value = eval_full(args[0], arena, params, row, hooks)?;
        if value.is_null() {
            return Ok(());
        }
        let Datum::Text(val_str) = value else {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "string_agg value must be text"
            ));
        };
        let sep = eval_full(args[1], arena, params, row, hooks)?;
        let sep_str = match sep {
            Datum::Text(s) => s,
            Datum::Null => "",
            _ => {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "string_agg delimiter must be text"
                ))
            }
        };
        // Stash the first delimiter so the DISTINCT/ORDER BY fold can reuse it.
        if self.sep.is_none() {
            self.sep = Some(sep_str);
        }
        if self.ordered {
            // Buffer `[value, sort-keys...]` to sort and concatenate in finish.
            let mut tuple = [Datum::Null; 1 + MAX_PROJ];
            tuple[0] = Datum::Text(val_str);
            for (i, o) in self.ord_spec.iter().enumerate() {
                tuple[1 + i] = eval_full(o.expression, arena, params, row, hooks)?;
            }
            let enc =
                crate::sql::exec::encode_projected_pub(&tuple[..1 + self.ord_spec.len()], arena)?;
            // DISTINCT (the sort key is the value itself, enforced in init):
            // encoded-tuple equality is value equality, so skip duplicates.
            if self.distinct && self.ord_len > 0 {
                let seen = unsafe { core::slice::from_raw_parts(self.ord, self.ord_len) };
                if seen.contains(&enc) {
                    return Ok(());
                }
            }
            self.push_ordered(enc, arena)?;
            self.count += 1;
            return Ok(());
        }
        if self.distinct {
            return self.push_distinct(Datum::Text(val_str), arena);
        }
        self.append_str_elem(sep_str, val_str, arena)?;
        self.count += 1;
        Ok(())
    }

    /// Append an encoded `[value, keys...]` tuple to the ORDER BY buffer,
    /// growing it (doubling) in the arena when full.
    fn push_ordered(&mut self, enc: &'a [u8], arena: &'a Arena) -> Result<(), SqlError> {
        if self.ord_len == self.ord_cap {
            let new_cap = if self.ord_cap == 0 { 8 } else { self.ord_cap * 2 };
            let empty: &[u8] = &[];
            let fresh = arena
                .alloc_slice_with(new_cap, |_| empty)
                .map_err(|_| arena_full())?;
            if self.ord_len > 0 {
                let old = unsafe { core::slice::from_raw_parts(self.ord, self.ord_len) };
                fresh[..self.ord_len].copy_from_slice(old);
            }
            self.ord = fresh.as_mut_ptr();
            self.ord_cap = new_cap;
        }
        unsafe { self.ord.add(self.ord_len).write(enc) };
        self.ord_len += 1;
        Ok(())
    }

    /// Append `value` to the string_agg buffer, prefixing `sep` for every element
    /// after the first (first = buffer still empty).
    fn append_str_elem(&mut self, sep: &str, value: &str, arena: &'a Arena) -> Result<(), SqlError> {
        if self.str_len > 0 {
            self.push_bytes(sep.as_bytes(), arena)?;
        }
        self.push_bytes(value.as_bytes(), arena)?;
        Ok(())
    }

    /// Append raw bytes to the string_agg buffer, growing it (doubling) in the
    /// arena when it would overflow.
    fn push_bytes(&mut self, src: &[u8], arena: &'a Arena) -> Result<(), SqlError> {
        let need = self.str_len + src.len();
        if need > self.str_cap {
            let mut new_cap = if self.str_cap == 0 { 16 } else { self.str_cap * 2 };
            while new_cap < need {
                new_cap *= 2;
            }
            let fresh = arena
                .alloc_slice_with(new_cap, |_| 0u8)
                .map_err(|_| arena_full())?;
            if self.str_len > 0 {
                let old = unsafe { core::slice::from_raw_parts(self.str_buf, self.str_len) };
                fresh[..self.str_len].copy_from_slice(old);
            }
            self.str_buf = fresh.as_mut_ptr();
            self.str_cap = new_cap;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), self.str_buf.add(self.str_len), src.len());
        }
        self.str_len += src.len();
        Ok(())
    }

    /// Append a non-null value to the DISTINCT buffer, growing it (doubling)
    /// in the arena when full. The prior region becomes dead bump-arena space.
    /// Reduces the buffered `WITHIN GROUP` values for an ordered-set aggregate.
    fn finish_ordered_set(&mut self, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        let n = self.vals_len;
        if n == 0 {
            return Ok(Datum::Null);
        }
        let values: &mut [Datum<'a>] = unsafe { core::slice::from_raw_parts_mut(self.vals, n) };
        // Stable insertion sort (compare_datums is fallible, so no library sort).
        for i in 1..n {
            let mut j = i;
            while j > 0 && compare_datums(&values[j - 1], &values[j])?.is_gt() {
                values.swap(j - 1, j);
                j -= 1;
            }
        }
        match self.kind {
            AggKind::Mode => {
                // Most frequent value; ties resolve to the smallest (first).
                let (mut best_index, mut best_run) = (0usize, 0usize);
                let mut i = 0;
                while i < n {
                    let mut end = i;
                    while end + 1 < n && compare_datums(&values[end], &values[end + 1])?.is_eq() {
                        end += 1;
                    }
                    if end - i + 1 > best_run {
                        best_run = end - i + 1;
                        best_index = i;
                    }
                    i = end + 1;
                }
                Ok(values[best_index])
            }
            AggKind::PercentileDisc => {
                let fraction = self.sum_float.clamp(0.0, 1.0);
                let index = if fraction <= 0.0 {
                    0
                } else {
                    ((fraction * n as f64).ceil() as usize).saturating_sub(1).min(n - 1)
                };
                Ok(values[index])
            }
            _ => {
                // PercentileCont: linear interpolation between the two nearest
                // ranks. Numeric input yields numeric; int/float yield double
                // precision (PostgreSQL's signatures).
                let fraction = self.sum_float.clamp(0.0, 1.0);
                let position = fraction * (n as f64 - 1.0);
                let low = position.floor() as usize;
                let high = position.ceil() as usize;
                let weight = position - low as f64;
                let to_f64 = |d: &Datum<'a>| -> f64 {
                    match d {
                        Datum::Int4(v) => f64::from(*v),
                        Datum::Int8(v) => *v as f64,
                        Datum::Float8(v) => *v,
                        Datum::Numeric(v) => v.to_f64(),
                        _ => 0.0,
                    }
                };
                let interpolated = to_f64(&values[low]) + (to_f64(&values[high]) - to_f64(&values[low])) * weight;
                match values[low] {
                    Datum::Numeric(_) => {
                        let text = crate::stack_format!(48, "{}", interpolated);
                        Ok(Datum::Numeric(crate::sql::numeric::Numeric::parse(text.as_str(), arena)?))
                    }
                    _ => Ok(Datum::Float8(interpolated)),
                }
            }
        }
    }

    fn push_distinct(&mut self, v: Datum<'a>, arena: &'a Arena) -> Result<(), SqlError> {
        if self.vals_len == self.vals_cap {
            let new_cap = if self.vals_cap == 0 { 8 } else { self.vals_cap * 2 };
            let fresh = arena
                .alloc_slice_with(new_cap, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            if self.vals_len > 0 {
                let old = unsafe { core::slice::from_raw_parts(self.vals, self.vals_len) };
                fresh[..self.vals_len].copy_from_slice(old);
            }
            self.vals = fresh.as_mut_ptr();
            self.vals_cap = new_cap;
        }
        unsafe { self.vals.add(self.vals_len).write(v) };
        self.vals_len += 1;
        Ok(())
    }

    /// Sort the DISTINCT buffer, drop adjacent duplicates, and fold the unique
    /// values through `accumulate` (bumping `count` per unique value). A no-operator
    /// for non-distinct aggregates.
    fn fold_distinct(&mut self, arena: &'a Arena) -> Result<(), SqlError> {
        if !self.distinct || self.vals_len == 0 {
            return Ok(());
        }
        let vals = unsafe { core::slice::from_raw_parts_mut(self.vals, self.vals_len) };
        let mut cmp_err: Option<SqlError> = None;
        vals.sort_unstable_by(|a, b| match compare_datums(a, b) {
            Ok(o) => o,
            Err(e) => {
                if cmp_err.is_none() {
                    cmp_err = Some(e);
                }
                core::cmp::Ordering::Equal
            }
        });
        if let Some(e) = cmp_err {
            return Err(e);
        }
        let mut prev: Option<Datum<'a>> = None;
        for &v in vals.iter() {
            let fresh = match prev {
                None => true,
                Some(p) => !compare_datums(&p, &v)?.is_eq(),
            };
            if fresh {
                self.count += 1;
                self.accumulate(v, arena)?;
                prev = Some(v);
            }
        }
        Ok(())
    }

    /// string_agg(x ORDER BY k): sort the buffered `[value, keys...]` tuples by
    /// the key columns (honoring ASC/DESC and NULLS placement) and concatenate
    /// the value column into the output buffer.
    fn fold_ordered(&mut self, arena: &'a Arena) -> Result<(), SqlError> {
        if !self.ordered || self.ord_len == 0 {
            return Ok(());
        }
        let rows = unsafe { core::slice::from_raw_parts_mut(self.ord, self.ord_len) };
        let spec = self.ord_spec;
        let mut cmp_err: Option<SqlError> = None;
        rows.sort_unstable_by(|a, b| {
            use core::cmp::Ordering;
            for (k, o) in spec.iter().enumerate() {
                let ka = crate::sql::exec::decode_projected_pub(a, 1 + k);
                let kb = crate::sql::exec::decode_projected_pub(b, 1 + k);
                let ord = match (ka.is_null(), kb.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => {
                        if o.nulls_first { Ordering::Less } else { Ordering::Greater }
                    }
                    (false, true) => {
                        if o.nulls_first { Ordering::Greater } else { Ordering::Less }
                    }
                    (false, false) => match compare_datums(&ka, &kb) {
                        Ok(c) => if o.descending { c.reverse() } else { c },
                        Err(e) => {
                            if cmp_err.is_none() {
                                cmp_err = Some(e);
                            }
                            Ordering::Equal
                        }
                    },
                };
                if !ord.is_eq() {
                    return ord;
                }
            }
            Ordering::Equal
        });
        if let Some(e) = cmp_err {
            return Err(e);
        }
        let sep = self.sep.unwrap_or("");
        for &row in rows.iter() {
            let Datum::Text(s) = crate::sql::exec::decode_projected_pub(row, 0) else {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "string_agg value must be text"
                ));
            };
            self.append_str_elem(sep, s, arena)?;
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        use crate::sql::numeric::{self as num, Numeric};
        // Ordered-set aggregates reduce their buffered values directly.
        if matches!(
            self.kind,
            AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode
        ) {
            return self.finish_ordered_set(arena);
        }
        if self.kind == AggKind::ArrayAgg {
            return self.finish_array_agg(arena);
        }
        if let AggKind::JsonAgg { jsonb } = self.kind {
            return self.finish_json_agg(jsonb, arena);
        }
        if let AggKind::JsonObjectAgg { jsonb } = self.kind {
            return self.finish_json_object_agg(jsonb, arena);
        }
        self.fold_distinct(arena)?;
        self.fold_ordered(arena)?;
        Ok(match self.kind {
            AggKind::Count => Datum::Int8(self.count as i64),
            // regr_count over an empty group is 0, not NULL.
            AggKind::RegrCount => Datum::Int8(self.count as i64),
            // Min/Max and the bitwise aggregates return the running value in
            // `best` (NULL for an all-NULL or empty group).
            AggKind::Min | AggKind::Max | AggKind::BitAnd | AggKind::BitOr | AggKind::BitXor => {
                self.best.unwrap_or(Datum::Null)
            }
            _ if self.count == 0 => Datum::Null,
            // Statistical aggregates (count >= 1 here). `Sxx`/`Syy`/`Sxy` are the
            // corrected sums of squares/products; `_samp` needs count >= 2.
            AggKind::VarPop
            | AggKind::VarSamp
            | AggKind::StddevPop
            | AggKind::StddevSamp
            | AggKind::Corr
            | AggKind::CovarPop
            | AggKind::CovarSamp
            | AggKind::RegrSlope
            | AggKind::RegrIntercept
            | AggKind::RegrR2
            | AggKind::RegrAvgx
            | AggKind::RegrAvgy
            | AggKind::RegrSxx
            | AggKind::RegrSyy
            | AggKind::RegrSxy => {
                // Single-argument variance/stddev over integer/numeric inputs
                // return an exact numeric result (PostgreSQL's numeric path);
                // float8 inputs and all two-argument aggregates fold in f64.
                if self.kind.is_one_arg_stat() && self.arg_kind != ArgKind::Float {
                    let sum_x = self.sum_numeric.unwrap_or(Numeric::ZERO);
                    let sum_x2 = self.sum_sq_numeric.unwrap_or(Numeric::ZERO);
                    let sample = matches!(self.kind, AggKind::VarSamp | AggKind::StddevSamp);
                    let want_stddev =
                        matches!(self.kind, AggKind::StddevPop | AggKind::StddevSamp);
                    return Ok(
                        match num::var_stddev(self.count, &sum_x, &sum_x2, sample, want_stddev, arena)? {
                            Some(result) => Datum::Numeric(result),
                            None => Datum::Null,
                        },
                    );
                }
                let n = self.count as f64;
                let sxx = self.sum_sq - self.sum_float * self.sum_float / n;
                let syy = self.sum_yy - self.sum_y * self.sum_y / n;
                let sxy = self.sum_xy - self.sum_float * self.sum_y / n;
                let samp_ok = self.count >= 2;
                let value = match self.kind {
                    AggKind::VarPop => Some(sxx / n),
                    AggKind::VarSamp => samp_ok.then(|| sxx / (n - 1.0)),
                    AggKind::StddevPop => Some((sxx / n).max(0.0).sqrt()),
                    AggKind::StddevSamp => samp_ok.then(|| (sxx / (n - 1.0)).max(0.0).sqrt()),
                    AggKind::CovarPop => Some(sxy / n),
                    AggKind::CovarSamp => samp_ok.then(|| sxy / (n - 1.0)),
                    AggKind::Corr => {
                        if sxx == 0.0 || syy == 0.0 {
                            None
                        } else {
                            Some(sxy / (sxx * syy).sqrt())
                        }
                    }
                    AggKind::RegrSlope => (sxx != 0.0).then(|| sxy / sxx),
                    AggKind::RegrIntercept => {
                        (sxx != 0.0).then(|| self.sum_y / n - (sxy / sxx) * (self.sum_float / n))
                    }
                    AggKind::RegrR2 => {
                        if sxx == 0.0 {
                            None
                        } else if syy == 0.0 {
                            Some(1.0)
                        } else {
                            Some(sxy * sxy / (sxx * syy))
                        }
                    }
                    AggKind::RegrAvgx => Some(self.sum_float / n),
                    AggKind::RegrAvgy => Some(self.sum_y / n),
                    AggKind::RegrSxx => Some(sxx),
                    AggKind::RegrSyy => Some(syy),
                    AggKind::RegrSxy => Some(sxy),
                    _ => None,
                };
                value.map_or(Datum::Null, Datum::Float8)
            }
            // SUM result type: int4->int8, int8->numeric, numeric->numeric,
            // float8->float8 (PostgreSQL's aggregate signatures).
            AggKind::Sum => match self.arg_kind {
                ArgKind::Float => Datum::Float8(self.sum_float),
                ArgKind::Int4 => Datum::Int8(
                    i64::try_from(self.sum_int)
                        .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "bigint out of range"))?,
                ),
                ArgKind::Int8 => Datum::Numeric(Numeric::from_i128(self.sum_int, arena)?),
                ArgKind::Numeric => {
                    Datum::Numeric(self.sum_numeric.unwrap_or(Numeric::ZERO))
                }
                ArgKind::None => Datum::Null,
            },
            // AVG: numeric for int/int8/numeric, float8 for float8.
            AggKind::Avg => match self.arg_kind {
                ArgKind::Float => Datum::Float8(self.sum_float / self.count as f64),
                ArgKind::Int4 | ArgKind::Int8 => {
                    let sum = Numeric::from_i128(self.sum_int, arena)?;
                    let cnt = Numeric::from_i64(self.count as i64, arena)?;
                    Datum::Numeric(num::div(&sum, &cnt, arena)?)
                }
                ArgKind::Numeric => {
                    let sum = self.sum_numeric.unwrap_or(Numeric::ZERO);
                    let cnt = Numeric::from_i64(self.count as i64, arena)?;
                    Datum::Numeric(num::div(&sum, &cnt, arena)?)
                }
                ArgKind::None => Datum::Null,
            },
            AggKind::BoolAnd | AggKind::BoolOr => match self.bool_acc {
                Some(v) => Datum::Bool(v),
                None => Datum::Null,
            },
            AggKind::StringAgg => {
                let bytes = unsafe { core::slice::from_raw_parts(self.str_buf, self.str_len) };
                Datum::Text(unsafe { core::str::from_utf8_unchecked(bytes) })
            }
            // Handled by `finish_ordered_set` before this match.
            AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode => Datum::Null,
            // Handled by `finish_array_agg` / `finish_json_agg` before this
            // match.
            AggKind::ArrayAgg | AggKind::JsonAgg { .. } | AggKind::JsonObjectAgg { .. } => {
                Datum::Null
            }
        })
    }

    /// Builds the `array_agg` result: the buffered values in ORDER BY order
    /// (or scan order), DISTINCT-deduped if requested. Zero rows → NULL (as
    /// PostgreSQL). The element type comes from the first non-null value.
    fn finish_array_agg(&mut self, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        let values = self.collect_agg_values(arena)?;
        if values.is_empty() {
            return Ok(Datum::Null);
        }
        // No default here. Falling back to int4 for an element type arrays
        // cannot yet carry relabelled the values rather than failing, so
        // `array_agg` over a time or a uuid came back as meaningless integers.
        let Some(element) = values.iter().find_map(crate::sql::types::ArrElem::from_datum) else {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "array_agg over {} is not supported yet",
                crate::sql::eval::type_name_of_pub(&values[0])
            ));
        };
        let raw = crate::sql::array::build(values, arena)?;
        Ok(Datum::Array { element, raw })
    }

    /// `json_agg`/`jsonb_agg`: the buffered values (ORDER BY / DISTINCT / scan
    /// order) as a JSON array. Zero rows → NULL. json uses `, ` element
    /// spacing (and `" : "` for nested objects); jsonb the canonical form.
    fn finish_json_agg(&mut self, jsonb: bool, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        let values = self.collect_agg_values(arena)?;
        if values.is_empty() {
            return Ok(Datum::Null);
        }
        let colon = if jsonb { ": " } else { " : " };
        let mut buf = crate::util::StackStr::<65536>::default();
        let _ = buf.write_char('[');
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                let _ = buf.write_str(", ");
            }
            let _ = crate::sql::json::write_datum_json_styled(v, colon, ", ", &mut buf);
        }
        let _ = buf.write_char(']');
        let text = arena.alloc_str(buf.as_str()).map_err(|_| arena_full())?;
        Ok(Datum::Json { text, jsonb })
    }

    /// `json_object_agg`/`jsonb_object_agg`: the buffered `[key, value]` tuples
    /// as a JSON object. Zero rows → NULL. json output is wrapped `{ … }` with
    /// `" : "` between key and value; jsonb is the canonical `{…}` / `": "`.
    fn finish_json_object_agg(
        &mut self,
        jsonb: bool,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        if self.ord_len == 0 {
            return Ok(Datum::Null);
        }
        let rows = unsafe { core::slice::from_raw_parts(self.ord, self.ord_len) };
        let colon = if jsonb { ": " } else { " : " };
        let (open, close) = if jsonb { ("{", "}") } else { ("{ ", " }") };
        let mut buf = crate::util::StackStr::<65536>::default();
        let _ = buf.write_str(open);
        for (i, &enc) in rows.iter().enumerate() {
            if i > 0 {
                let _ = buf.write_str(", ");
            }
            let key = crate::sql::exec::decode_projected_pub(enc, 0);
            let value = crate::sql::exec::decode_projected_pub(enc, 1);
            let mut key_text = crate::util::StackStr::<4096>::default();
            let _ = write!(key_text, "{key}");
            let _ = crate::sql::json::write_json_raw_string(key_text.as_str(), &mut buf);
            let _ = buf.write_str(colon);
            let _ = crate::sql::json::write_datum_json_styled(&value, colon, ", ", &mut buf);
        }
        let _ = buf.write_str(close);
        let text = arena.alloc_str(buf.as_str()).map_err(|_| arena_full())?;
        Ok(Datum::Json { text, jsonb })
    }

    /// The buffered aggregate values in ORDER BY / DISTINCT / scan order,
    /// shared by `finish_array_agg` and `finish_json_agg`.
    fn collect_agg_values(&mut self, arena: &'a Arena) -> Result<&'a [Datum<'a>], SqlError> {
        if self.ordered {
            let rows = unsafe { core::slice::from_raw_parts_mut(self.ord, self.ord_len) };
            let spec = self.ord_spec;
            let mut cmp_err: Option<SqlError> = None;
            rows.sort_by(|a, b| {
                use core::cmp::Ordering;
                for (k, o) in spec.iter().enumerate() {
                    let ka = crate::sql::exec::decode_projected_pub(a, 1 + k);
                    let kb = crate::sql::exec::decode_projected_pub(b, 1 + k);
                    let ord = match (ka.is_null(), kb.is_null()) {
                        (true, true) => Ordering::Equal,
                        (true, false) => if o.nulls_first { Ordering::Less } else { Ordering::Greater },
                        (false, true) => if o.nulls_first { Ordering::Greater } else { Ordering::Less },
                        (false, false) => match compare_datums(&ka, &kb) {
                            Ok(c) => if o.descending { c.reverse() } else { c },
                            Err(e) => {
                                if cmp_err.is_none() { cmp_err = Some(e); }
                                Ordering::Equal
                            }
                        },
                    };
                    if !ord.is_eq() {
                        return ord;
                    }
                }
                Ordering::Equal
            });
            if let Some(e) = cmp_err {
                return Err(e);
            }
            let out = arena.alloc_slice_with(rows.len(), |_| Datum::Null).map_err(|_| arena_full())?;
            for (i, &row) in rows.iter().enumerate() {
                out[i] = crate::sql::exec::decode_projected_pub(row, 0);
            }
            Ok(out)
        } else if self.distinct {
            let vals = unsafe { core::slice::from_raw_parts_mut(self.vals, self.vals_len) };
            let mut cmp_err: Option<SqlError> = None;
            vals.sort_by(|a, b| match compare_datums(a, b) {
                Ok(o) => o,
                Err(e) => {
                    if cmp_err.is_none() { cmp_err = Some(e); }
                    core::cmp::Ordering::Equal
                }
            });
            if let Some(e) = cmp_err {
                return Err(e);
            }
            let mut unique = 0usize;
            for i in 0..vals.len() {
                let same = i > 0 && compare_datums(&vals[i], &vals[i - 1])?.is_eq();
                if !same {
                    vals[unique] = vals[i];
                    unique += 1;
                }
            }
            Ok(&vals[..unique])
        } else {
            Ok(unsafe { core::slice::from_raw_parts(self.vals, self.vals_len) })
        }
    }
}
