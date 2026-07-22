//! Set-returning functions: the ones written in the select list, and the ones
//! written in FROM.
//!
//! A set-returning function in the select list makes each input row produce as
//! many output rows as the function yields, so the projection has to know how
//! many that is before it can emit any of them. In FROM the same function is a
//! table, and needs a definition — column names and types — that the ordinary
//! scan machinery can resolve against, which is what is synthesized here.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, Select, SelectItem, TableRef};
use crate::sql::eval::{eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError};
use crate::sql::exec::{describe_items, MAX_PROJ};

/// Pieces one `string_to_table` call may split into.
const MAX_PIECES: usize = 1024;
use crate::sql::types::{ColDesc, ColType, Datum};
use crate::sql_err;
use crate::storage::{ColumnMeta, SqlName, Storage, TableDef, MAX_COLUMNS};

use super::setops::describe_set_body;
use super::subquery::subquery_witness;

use super::{
    arena_full, describe_scope_items, record_star_width, QueryScope,
};

/// Whether `name` is one of the supported set-returning functions.
pub(super) fn is_srf_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("_pg_expandarray")
        || name.eq_ignore_ascii_case("unnest")
        || name.eq_ignore_ascii_case("generate_series")
        || name.eq_ignore_ascii_case("regexp_matches")
        || name.eq_ignore_ascii_case("jsonb_object_keys")
        || name.eq_ignore_ascii_case("json_object_keys")
        || name.eq_ignore_ascii_case("jsonb_array_elements")
        || name.eq_ignore_ascii_case("json_array_elements")
        || name.eq_ignore_ascii_case("jsonb_array_elements_text")
        || name.eq_ignore_ascii_case("json_array_elements_text")
        || name.eq_ignore_ascii_case("regexp_split_to_table")
        || name.eq_ignore_ascii_case("string_to_table")
        || name.eq_ignore_ascii_case("generate_subscripts")
        || is_json_each_name(name)
}

/// The set-returning function call (if any) driving a single expression's
/// expansion — the outermost SRF reachable through wrapping expressions.
pub(super) fn srf_in_expr<'a>(e: &'a Expr<'a>) -> Option<&'a Expr<'a>> {
    match e {
        Expr::Call { name, .. } if is_srf_name(name) => Some(e),
        Expr::Field { base, .. } => srf_in_expr(base),
        Expr::Cast { operand, .. } => srf_in_expr(operand),
        Expr::Unary { operand, .. } => srf_in_expr(operand),
        Expr::Binary { left, right, .. } => srf_in_expr(left).or_else(|| srf_in_expr(right)),
        Expr::Call { args, .. } => args.iter().find_map(|a| srf_in_expr(a)),
        _ => None,
    }
}

/// The SRF (if any) driving a single select item's expansion.
pub(super) fn srf_in_item<'a>(item: &'a SelectItem<'a>) -> Option<&'a Expr<'a>> {
    match item {
        SelectItem::Expr { expression, .. } => srf_in_expr(expression),
        SelectItem::RecordStar(base) => srf_in_expr(base),
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => None,
    }
}

/// Finds a set-returning function call among the SELECT items (the whole call
/// node, so the caller can compute its row count), or None for a single row.
pub(super) fn find_srf<'a>(items: &'a [SelectItem<'a>]) -> Option<&'a Expr<'a>> {
    items.iter().find_map(srf_in_item)
}

/// The number of output rows a select list's set-returning functions expand to:
/// the maximum length over all of them (each shorter one NULL-pads), matching
/// PostgreSQL's lockstep evaluation. Returns 1 when there is no SRF.
pub(super) fn srf_max_count<'a, R: ColumnLookup<'a>>(
    items: &'a [SelectItem<'a>],
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<usize, SqlError> {
    let mut any = false;
    let mut max = 0usize;
    for item in items {
        if let Some(call) = srf_in_item(item) {
            max = max.max(srf_count(call, arena, params, row, hooks)?);
            any = true;
        }
    }
    Ok(if any { max } else { 1 })
}

/// Number of output rows a set-returning call yields for the current source row.
pub(super) fn srf_count<'a, R: ColumnLookup<'a>>(
    call: &'a Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<usize, SqlError> {
    let Expr::Call { name, args, .. } = call else {
        return Ok(1);
    };
    let as_i64 = |d: &Datum| -> Option<i64> {
        match d {
            Datum::Int4(v) => Some(*v as i64),
            Datum::Int8(v) => Some(*v),
            _ => None,
        }
    };
    if name.eq_ignore_ascii_case("generate_series") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "generate_series(...) argument count"));
        }
        let start = eval_full(args[0], arena, params, row, hooks)?;
        let stop = eval_full(args[1], arena, params, row, hooks)?;
        let step = if args.len() == 3 {
            eval_full(args[2], arena, params, row, hooks)?
        } else {
            Datum::Int4(1)
        };
        if start.is_null() || stop.is_null() || step.is_null() {
            return Ok(0);
        }
        if let (Some(s), Some(e), Some(st)) = (as_i64(&start), as_i64(&stop), as_i64(&step)) {
            if st == 0 {
                return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
            }
            let n = if st > 0 {
                if e < s { 0 } else { (e - s) / st + 1 }
            } else if e > s {
                0
            } else {
                (s - e) / (-st) + 1
            };
            return Ok(n as usize);
        }
        // Temporal series: date/timestamp[tz] bounds with an interval step.
        // Coerce bare string literals for the stop and step, as PostgreSQL's
        // function resolution does.
        let Some((base, kind)) = crate::sql::eval::timestamp_series_start(&start) else {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "generate_series is supported for integer and timestamp arguments"
            ));
        };
        let stop = crate::sql::eval::cast_to(stop, kind.coltype(), arena)?;
        let step = crate::sql::eval::cast_to(step, ColType::Interval, arena)?;
        let (Some((stop_micros, _)), Datum::Interval(step_iv)) =
            (crate::sql::eval::timestamp_series_start(&stop), step)
        else {
            return Ok(0);
        };
        crate::sql::eval::timestamp_series_count(base, stop_micros, step_iv)
    } else if name.eq_ignore_ascii_case("regexp_matches") {
        // Number of matches: 0/1 without the `g` flag, else all non-overlapping.
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "regexp_matches(...) argument count"));
        }
        let string = eval_full(args[0], arena, params, row, hooks)?;
        let pattern = eval_full(args[1], arena, params, row, hooks)?;
        let (Datum::Text(string), Datum::Text(pattern)) = (string, pattern) else {
            return Ok(0);
        };
        let flags = if args.len() == 3 {
            match eval_full(args[2], arena, params, row, hooks)? {
                Datum::Text(f) => f,
                Datum::Null => return Ok(0),
                _ => "",
            }
        } else {
            ""
        };
        let (global, ci) = crate::sql::eval::regexp_flags(flags)?;
        let mut spans = [(-1i64, -1i64); crate::sql::regex::MAX_GROUPS];
        let mut from = 0usize;
        let mut n = 0usize;
        while let Some(((mstart, mend), _)) =
            crate::sql::regex::find_captures(pattern, string, from, ci, &mut spans)?
        {
            n += 1;
            if !global {
                break;
            }
            from = if mend > mstart { mend } else { mend + 1 };
            if from > string.len() {
                break;
            }
        }
        Ok(n)
    } else if name.eq_ignore_ascii_case("jsonb_object_keys")
        || name.eq_ignore_ascii_case("json_object_keys")
    {
        let text = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => return Err(crate::sql::json::object_keys_error(name, crate::sql::json::Kind::Scalar)),
        };
        let kind = crate::sql::json::kind_of(text);
        if kind != crate::sql::json::Kind::Object {
            return Err(crate::sql::json::object_keys_error(name, kind));
        }
        if name.eq_ignore_ascii_case("jsonb_object_keys") {
            return match crate::sql::json::parse(text, arena)? {
                crate::sql::json::Json::Object(members) => Ok(members.len()),
                _ => Err(crate::sql::json::object_keys_error(name, kind)),
            };
        }
        Ok(crate::sql::json::object_members_source(text, arena)?.len())
    } else if name.eq_ignore_ascii_case("jsonb_array_elements")
        || name.eq_ignore_ascii_case("json_array_elements")
        || name.eq_ignore_ascii_case("jsonb_array_elements_text")
        || name.eq_ignore_ascii_case("json_array_elements_text")
    {
        let jsonb = name.eq_ignore_ascii_case("jsonb_array_elements")
            || name.eq_ignore_ascii_case("jsonb_array_elements_text");
        let text = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => return Err(crate::sql::json::array_elements_error(name, jsonb, crate::sql::json::Kind::Scalar)),
        };
        let kind = crate::sql::json::kind_of(text);
        if kind != crate::sql::json::Kind::Array {
            return Err(crate::sql::json::array_elements_error(name, jsonb, kind));
        }
        if jsonb {
            return match crate::sql::json::parse(text, arena)? {
                crate::sql::json::Json::Array(items) => Ok(items.len()),
                _ => Err(crate::sql::json::array_elements_error(name, jsonb, kind)),
            };
        }
        Ok(crate::sql::json::array_elements_source(text, arena)?.len())
    } else if is_json_each_name(name) {
        let jsonb = name.eq_ignore_ascii_case("jsonb_each")
            || name.eq_ignore_ascii_case("jsonb_each_text");
        let as_text = name.eq_ignore_ascii_case("json_each_text")
            || name.eq_ignore_ascii_case("jsonb_each_text");
        let text = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(0),
            _ => return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "cannot deconstruct a scalar")),
        };
        Ok(crate::sql::eval::json_each_pairs(text, jsonb, as_text, arena)?.len())
    } else if name.eq_ignore_ascii_case("regexp_split_to_table") {
        let (src, pat) = match (
            eval_full(args[0], arena, params, row, hooks)?,
            eval_full(args[1], arena, params, row, hooks)?,
        ) {
            (Datum::Text(s), Datum::Text(p)) => (s, p),
            _ => return Ok(0),
        };
        let ci = if args.len() == 3 {
            match eval_full(args[2], arena, params, row, hooks)? {
                Datum::Text(f) => crate::sql::eval::regexp_flags(f)?.1,
                _ => return Ok(0),
            }
        } else {
            false
        };
        Ok(crate::sql::eval::regex_split_pub(src, pat, ci, arena)?.len())
    } else if name.eq_ignore_ascii_case("string_to_table") {
        let (src, delimiter) = match (
            eval_full(args[0], arena, params, row, hooks)?,
            eval_full(args[1], arena, params, row, hooks)?,
        ) {
            (Datum::Text(s), Datum::Text(d)) => (s, Some(d)),
            // A NULL delimiter splits into characters; a NULL input yields nothing.
            (Datum::Text(s), Datum::Null) => (s, None),
            _ => return Ok(0),
        };
        let mut pieces: [&str; MAX_PIECES] = [""; MAX_PIECES];
        Ok(crate::sql::eval::split_pieces(src, delimiter, &mut pieces)?)
    } else if name.eq_ignore_ascii_case("generate_subscripts") {
        let raw = match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Array { raw, .. } => raw,
            _ => return Ok(0),
        };
        let dim = match eval_full(args[1], arena, params, row, hooks)? {
            Datum::Int4(v) => v as i64,
            Datum::Int8(v) => v,
            _ => return Ok(0),
        };
        Ok(if dim == 1 { crate::sql::array::len(raw) } else { 0 })
    } else {
        // unnest / _pg_expandarray over an array.
        match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Array { raw, .. } => Ok(crate::sql::array::len(raw)),
            Datum::Null => Ok(0),
            _ => Ok(1),
        }
    }
}

/// Synthesizes a `TableDef` for a derived table (`FROM (SELECT ...) exposed`)
/// from the subquery's output column names and inferred types. Schema only —
/// no rows are produced, so it needs neither a txid nor bound parameters.
pub(super) fn synth_derived_def<'a>(
    storage: &'a Storage,
    sub: &'a Select<'a>,
    exposed: &'a str,
    col_alias: Option<&'a [&'a str]>,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a TableDef, SqlError> {
    let mut descriptors = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match sub.set_body {
        Some(tree) => describe_set_body(storage, tree, txid, &mut descriptors, arena)?,
        None => match &sub.from {
            Some(f) => {
                let ss = QueryScope::resolve_schema(storage, f, txid, arena)?;
                let n = describe_scope_items(sub.items, &ss, &mut descriptors)?;
                // A bare scalar/array subquery item (possibly correlated) has
                // no static type from the scope and describes as text; infer
                // its real type from the inner select's projection so the
                // derived-table column is typed correctly.
                let mut slot = 0usize;
                for item in sub.items {
                    match item {
                        SelectItem::Wildcard => slot += ss.star_columns(),
                        SelectItem::TableWildcard(q) => {
                            slot += ss.defs[ss.table_index(q)?].expect("resolved").n_columns;
                        }
                        SelectItem::RecordStar(base) => slot += record_star_width(base, &ss),
                        SelectItem::Expr { expression, .. } => {
                            if slot < n
                                && descriptors[slot].type_oid == crate::sql::types::oid::TEXT
                                && let Expr::Subquery(inner_sub) = &**expression
                                && let Some(SelectItem::Expr { expression: inner, .. }) =
                                    inner_sub.items.first()
                            {
                                let inner_scope = inner_sub.from.as_ref().and_then(|inf| {
                                    QueryScope::resolve_schema(storage, inf, txid, arena).ok()
                                });
                                let witness = subquery_witness(
                                    inner,
                                    inner_scope.as_ref().or(Some(&ss)),
                                );
                                if !witness.is_null() {
                                    descriptors[slot] = ColDesc::new(
                                        descriptors[slot].name,
                                        witness.type_oid(),
                                        -1,
                                    );
                                }
                            }
                            slot += 1;
                        }
                    }
                }
                n
            }
            None => describe_items(sub.items, None, &mut descriptors)?,
        },
    };
    if n_cols > MAX_COLUMNS {
        return Err(sql_err!(
            "54011",
            "derived table \"{}\" has too many columns",
            exposed
        ));
    }
    // A column-alias list renames the output columns; PostgreSQL requires it to
    // supply no more names than the derived table has columns.
    if let Some(aliases) = col_alias {
        if aliases.len() > n_cols {
            return Err(sql_err!(
                "42P10",
                "table \"{}\" has {} columns available but {} columns specified",
                exposed,
                n_cols,
                aliases.len()
            ));
        }
        for (i, alias) in aliases.iter().enumerate() {
            descriptors[i].name = alias;
        }
    }
    let blank = ColumnMeta {
        name: SqlName::parse("").expect("empty name is valid"),
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
    };
    let mut columns = [blank; MAX_COLUMNS];
    for i in 0..n_cols {
        let ct = crate::sql::exec::coltype_of_oid(descriptors[i].type_oid).ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "derived table column \"{}\" type (oid {}) is not supported",
                descriptors[i].name,
                descriptors[i].type_oid
            )
        })?;
        columns[i] = ColumnMeta {
            name: SqlName::parse(descriptors[i].name)?,
            ctype: ct,
            ..blank
        };
    }
    let def = TableDef {
        name: SqlName::parse(exposed)?,
        columns,
        n_columns: n_cols,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(def).map_err(|_| arena_full())?)
}

/// Synthesizes the single-column `TableDef` for a supported table function
/// (`FROM func(args) alias`). The output column is named after the alias (or the
/// function name), so a bare reference to the alias resolves to the value.
pub(super) fn table_func_def<'a>(
    tref: &'a TableRef<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a TableDef, SqlError> {
    let is_gs = tref.table.eq_ignore_ascii_case("generate_series");
    let is_unnest = tref.table.eq_ignore_ascii_case("unnest");
    let is_re = tref.table.eq_ignore_ascii_case("regexp_matches");
    let is_keys = tref.table.eq_ignore_ascii_case("jsonb_object_keys")
        || tref.table.eq_ignore_ascii_case("json_object_keys");
    let is_elems = tref.table.eq_ignore_ascii_case("jsonb_array_elements")
        || tref.table.eq_ignore_ascii_case("json_array_elements")
        || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
        || tref.table.eq_ignore_ascii_case("json_array_elements_text");
    let is_each = is_json_each_name(tref.table);
    let is_rstt = tref.table.eq_ignore_ascii_case("regexp_split_to_table");
    let is_gsub = tref.table.eq_ignore_ascii_case("generate_subscripts");
    let is_stt = tref.table.eq_ignore_ascii_case("string_to_table");
    if !is_gs && !is_unnest && !is_re && !is_keys && !is_elems && !is_each && !is_rstt && !is_gsub && !is_stt
    {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "table function \"{}\" is not supported",
            tref.table
        ));
    }
    let base_name = if is_gs {
        "generate_series"
    } else if is_re {
        "regexp_matches"
    } else if is_keys || is_elems || is_each || is_rstt || is_gsub || is_stt {
        tref.table
    } else {
        "unnest"
    };
    let name = tref.alias.unwrap_or(base_name);
    let blank = ColumnMeta {
        name: SqlName::parse("").expect("empty name is valid"),
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
    };
    // Each supported function's output columns: `key`/`value` for the `each`
    // family (two columns), a single column named per the function otherwise.
    // generate_series yields int8; regexp_matches yields text[]; unnest yields
    // the array's element type; array_elements' default column is `value`.
    let mut default_cols: [(&str, ColType); 2] = [("", ColType::Bool); 2];
    let n_default = if is_each {
        let value_type = if tref.table.eq_ignore_ascii_case("json_each") {
            ColType::Json
        } else if tref.table.eq_ignore_ascii_case("jsonb_each") {
            ColType::Jsonb
        } else {
            ColType::Text // json_each_text / jsonb_each_text
        };
        default_cols[0] = ("key", ColType::Text);
        default_cols[1] = ("value", value_type);
        2
    } else {
        let single_type = if is_gs {
            // Integer series → int8; a date/timestamp start makes it temporal.
            match tref.func_args.and_then(|a| a.first()) {
                Some(e) => match crate::sql::eval::eval(e, arena, params, &crate::sql::eval::NoColumns)? {
                    Datum::Timestamp(_) => ColType::Timestamp,
                    Datum::Timestamptz(_) | Datum::Date(_) => ColType::Timestamptz,
                    _ => ColType::Int8,
                },
                None => ColType::Int8,
            }
        } else if is_gsub {
            ColType::Int4
        } else if is_re {
            ColType::Array(crate::sql::types::ArrElem::Text)
        } else if is_keys || is_rstt || is_stt {
            ColType::Text
        } else if is_elems {
            if tref.table.eq_ignore_ascii_case("json_array_elements") {
                ColType::Json
            } else if tref.table.eq_ignore_ascii_case("jsonb_array_elements") {
                ColType::Jsonb
            } else {
                ColType::Text
            }
        } else {
            let args = tref.func_args.unwrap_or(&[]);
            match args.first() {
                Some(e) => match crate::sql::eval::eval(e, arena, params, &crate::sql::eval::NoColumns)? {
                    Datum::Array { element, .. } => element.to_coltype(),
                    _ => ColType::Text,
                },
                None => ColType::Text,
            }
        };
        // A single-column function's default column name is `value` for
        // array_elements, else the (aliased) function name.
        default_cols[0] = (if is_elems { "value" } else { name }, single_type);
        1
    };
    // `WITH ORDINALITY` appends a `bigint` ordinality column.
    let n_out = if tref.with_ordinality { n_default + 1 } else { n_default };
    // Column aliases rename the columns positionally; too many is an error.
    if let Some(aliases) = tref.col_alias
        && aliases.len() > n_out
    {
        return Err(sql_err!(
            "42P10",
            "table \"{}\" has {} columns available but {} columns specified",
            name,
            n_out,
            aliases.len()
        ));
    }
    let mut columns = [blank; MAX_COLUMNS];
    for (i, (default_name, ctype)) in default_cols[..n_default].iter().enumerate() {
        let col_name = tref
            .col_alias
            .and_then(|a| a.get(i).copied())
            .unwrap_or(default_name);
        columns[i] = ColumnMeta { name: SqlName::parse(col_name)?, ctype: *ctype, ..blank };
    }
    if tref.with_ordinality {
        let col_name = tref
            .col_alias
            .and_then(|a| a.get(n_default).copied())
            .unwrap_or("ordinality");
        columns[n_default] = ColumnMeta { name: SqlName::parse(col_name)?, ctype: ColType::Int8, ..blank };
    }
    let def = TableDef {
        name: SqlName::parse(name)?,
        columns,
        n_columns: n_out,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(def).map_err(|_| arena_full())?)
}

/// Whether `name` is one of the two-column `json_each` set-returning functions.
pub(super) fn is_json_each_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("json_each")
        || name.eq_ignore_ascii_case("jsonb_each")
        || name.eq_ignore_ascii_case("json_each_text")
        || name.eq_ignore_ascii_case("jsonb_each_text")
}

/// Materializes a table function's rows. Currently `generate_series(start, stop
/// [, step])` over integers; the arguments are evaluated as constants (a lateral
/// argument referencing an outer column surfaces loudly as an unresolved
/// column). Each row is one `int8` value, projected-encoded.
pub(super) fn table_func_rows<'a>(
    tref: &'a TableRef<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a [&'a [u8]], SqlError> {
    let args = tref.func_args.expect("table function carries arguments");
    // json_each / jsonb_each[_text]: one (key, value) row per object member.
    if is_json_each_name(tref.table) {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_each")
            || tref.table.eq_ignore_ascii_case("jsonb_each_text");
        let as_text = tref.table.eq_ignore_ascii_case("json_each_text")
            || tref.table.eq_ignore_ascii_case("jsonb_each_text");
        let text = match crate::sql::eval::eval(args[0], arena, params, &crate::sql::eval::NoColumns)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "cannot deconstruct a scalar")),
        };
        let pairs = crate::sql::eval::json_each_pairs(text, jsonb, as_text, arena)?;
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(pairs.len(), |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, (key, value)) in rows.iter_mut().zip(pairs.iter()) {
            *slot = crate::sql::exec::encode_projected_pub(&[Datum::Text(key), *value], arena)?;
        }
        return Ok(&*rows);
    }
    // string_to_table(string, delimiter [, null_string]): one text row per
    // piece. A NULL delimiter splits into characters and a piece equal to
    // null_string is NULL, both as `string_to_array` has it — the split rule
    // itself is shared, so the two cannot drift apart.
    if tref.table.eq_ignore_ascii_case("string_to_table") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function string_to_table(...) with {} arguments does not exist",
                args.len()
            ));
        }
        let evaluate = |i: usize| {
            crate::sql::eval::eval(args[i], arena, params, &crate::sql::eval::NoColumns)
        };
        let source = match evaluate(0)? {
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            a => return Err(crate::sql::eval::type_mismatch_pub("string_to_table", &a)),
        };
        let delimiter = match evaluate(1)? {
            Datum::Text(d) => Some(d),
            Datum::Null => None,
            a => return Err(crate::sql::eval::type_mismatch_pub("string_to_table", &a)),
        };
        let null_string = if args.len() == 3 {
            match evaluate(2)? {
                Datum::Text(t) => Some(t),
                Datum::Null => None,
                a => return Err(crate::sql::eval::type_mismatch_pub("string_to_table", &a)),
            }
        } else {
            None
        };
        let mut pieces: [&str; MAX_PIECES] = [""; MAX_PIECES];
        let n = crate::sql::eval::split_pieces(source, delimiter, &mut pieces)?;
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(n, |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, piece) in rows.iter_mut().zip(pieces[..n].iter()) {
            let value = if null_string == Some(*piece) {
                Datum::Null
            } else {
                Datum::Text(piece)
            };
            *slot = crate::sql::exec::encode_projected_pub(&[value], arena)?;
        }
        return Ok(&*rows);
    }
    // regexp_split_to_table(string, pattern [, flags]): one text row per piece.
    if tref.table.eq_ignore_ascii_case("regexp_split_to_table") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "regexp_split_to_table(...) argument count"));
        }
        let (src, pat) = match (
            crate::sql::eval::eval(args[0], arena, params, &crate::sql::eval::NoColumns)?,
            crate::sql::eval::eval(args[1], arena, params, &crate::sql::eval::NoColumns)?,
        ) {
            (Datum::Text(s), Datum::Text(p)) => (s, p),
            (Datum::Null, _) | (_, Datum::Null) => return Ok(&[]),
            (a, _) => return Err(crate::sql::eval::type_mismatch_pub("regexp_split_to_table", &a)),
        };
        let case_insensitive = if args.len() == 3 {
            match crate::sql::eval::eval(args[2], arena, params, &crate::sql::eval::NoColumns)? {
                Datum::Text(f) => crate::sql::eval::regexp_flags(f)?.1,
                Datum::Null => return Ok(&[]),
                _ => false,
            }
        } else {
            false
        };
        let pieces = crate::sql::eval::regex_split_pub(src, pat, case_insensitive, arena)?;
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(pieces.len(), |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, piece) in rows.iter_mut().zip(pieces.iter()) {
            *slot = crate::sql::exec::encode_projected_pub(&[*piece], arena)?;
        }
        return Ok(&*rows);
    }
    // generate_subscripts(array, dim): the 1-based indices of `array` along
    // `dim`; empty for a dim other than 1 (arrays are one-dimensional here).
    if tref.table.eq_ignore_ascii_case("generate_subscripts") {
        if args.len() != 2 {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "generate_subscripts(...) argument count"));
        }
        let raw = match crate::sql::eval::eval(args[0], arena, params, &crate::sql::eval::NoColumns)? {
            Datum::Array { raw, .. } => raw,
            Datum::Null => return Ok(&[]),
            a => return Err(crate::sql::eval::type_mismatch_pub("generate_subscripts", &a)),
        };
        let dim = match crate::sql::eval::eval(args[1], arena, params, &crate::sql::eval::NoColumns)? {
            Datum::Int4(v) => v as i64,
            Datum::Int8(v) => v,
            Datum::Null => return Ok(&[]),
            a => return Err(crate::sql::eval::type_mismatch_pub("generate_subscripts", &a)),
        };
        let count = if dim == 1 { crate::sql::array::len(raw) } else { 0 };
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
        for (i, slot) in rows.iter_mut().enumerate() {
            *slot = crate::sql::exec::encode_projected_pub(&[Datum::Int4((i + 1) as i32)], arena)?;
        }
        return Ok(&*rows);
    }
    // regexp_matches(string, pattern [, flags]): one row per match, each a
    // text[] of the capture groups (or the whole match when there are no groups).
    if tref.table.eq_ignore_ascii_case("regexp_matches") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "regexp_matches(...) argument count"));
        }
        let string = crate::sql::eval::eval(args[0], arena, params, &crate::sql::eval::NoColumns)?;
        let pattern = crate::sql::eval::eval(args[1], arena, params, &crate::sql::eval::NoColumns)?;
        let (Datum::Text(string), Datum::Text(pattern)) = (string, pattern) else {
            return Ok(&[]);
        };
        let flags = if args.len() == 3 {
            match crate::sql::eval::eval(args[2], arena, params, &crate::sql::eval::NoColumns)? {
                Datum::Text(f) => f,
                Datum::Null => return Ok(&[]),
                _ => "",
            }
        } else {
            ""
        };
        let (global, ci) = crate::sql::eval::regexp_flags(flags)?;
        // Collect each match's encoded text[] row.
        const EMPTY: &[u8] = &[];
        let mut rows = [EMPTY; crate::sql::parser::MAX_LIST];
        let mut n = 0usize;
        let mut spans = [(-1i64, -1i64); crate::sql::regex::MAX_GROUPS];
        let mut from = 0usize;
        while let Some(((mstart, mend), ng)) =
            crate::sql::regex::find_captures(pattern, string, from, ci, &mut spans)?
        {
            let mut elems = [Datum::Null; crate::sql::regex::MAX_GROUPS];
            let count = if ng == 0 {
                elems[0] = Datum::Text(&string[mstart..mend]);
                1
            } else {
                for (i, span) in spans[..ng].iter().enumerate() {
                    elems[i] = if span.0 < 0 {
                        Datum::Null
                    } else {
                        Datum::Text(&string[span.0 as usize..span.1 as usize])
                    };
                }
                ng
            };
            let arr = Datum::Array {
                element: crate::sql::types::ArrElem::Text,
                raw: crate::sql::array::build(&elems[..count], arena)?,
            };
            if n == crate::sql::parser::MAX_LIST {
                return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many regexp_matches rows"));
            }
            rows[n] = crate::sql::exec::encode_projected_pub(&[arr], arena)?;
            n += 1;
            if !global {
                break;
            }
            from = if mend > mstart { mend } else { mend + 1 };
            if from > string.len() {
                break;
            }
        }
        let out = arena.alloc_slice_with(n, |i| rows[i]).map_err(|_| arena_full())?;
        return Ok(&*out);
    }
    // jsonb_object_keys(obj) / json_object_keys(obj): one text row per key.
    if tref.table.eq_ignore_ascii_case("jsonb_object_keys")
        || tref.table.eq_ignore_ascii_case("json_object_keys")
    {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_object_keys");
        let text = match crate::sql::eval::eval(args[0], arena, params, &crate::sql::eval::NoColumns)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => return Err(crate::sql::json::object_keys_error(tref.table, crate::sql::json::Kind::Scalar)),
        };
        let kind = crate::sql::json::kind_of(text);
        if kind != crate::sql::json::Kind::Object {
            return Err(crate::sql::json::object_keys_error(tref.table, kind));
        }
        const EMPTY: &[u8] = &[];
        // jsonb: normalized/sorted keys; json: source order with duplicates.
        if jsonb {
            let crate::sql::json::Json::Object(members) = crate::sql::json::parse(text, arena)? else {
                return Err(crate::sql::json::object_keys_error(tref.table, kind));
            };
            let rows = arena.alloc_slice_with(members.len(), |_| EMPTY).map_err(|_| arena_full())?;
            for (slot, (key, _)) in rows.iter_mut().zip(members.iter()) {
                *slot = crate::sql::exec::encode_projected_pub(&[Datum::Text(key)], arena)?;
            }
            return Ok(&*rows);
        }
        let members = crate::sql::json::object_members_source(text, arena)?;
        let rows = arena.alloc_slice_with(members.len(), |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, (key, _)) in rows.iter_mut().zip(members.iter()) {
            *slot = crate::sql::exec::encode_projected_pub(&[Datum::Text(key)], arena)?;
        }
        return Ok(&*rows);
    }
    // jsonb_array_elements / json_array_elements[_text]: one row per element.
    if tref.table.eq_ignore_ascii_case("jsonb_array_elements")
        || tref.table.eq_ignore_ascii_case("json_array_elements")
        || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
        || tref.table.eq_ignore_ascii_case("json_array_elements_text")
    {
        let jsonb = tref.table.eq_ignore_ascii_case("jsonb_array_elements")
            || tref.table.eq_ignore_ascii_case("jsonb_array_elements_text");
        let as_text = tref.table.eq_ignore_ascii_case("jsonb_array_elements_text")
            || tref.table.eq_ignore_ascii_case("json_array_elements_text");
        let text = match crate::sql::eval::eval(args[0], arena, params, &crate::sql::eval::NoColumns)? {
            Datum::Json { text, .. } => text,
            Datum::Text(s) => s,
            Datum::Null => return Ok(&[]),
            _ => return Err(crate::sql::json::array_elements_error(tref.table, jsonb, crate::sql::json::Kind::Scalar)),
        };
        let kind = crate::sql::json::kind_of(text);
        if kind != crate::sql::json::Kind::Array {
            return Err(crate::sql::json::array_elements_error(tref.table, jsonb, kind));
        }
        const EMPTY: &[u8] = &[];
        if jsonb {
            let crate::sql::json::Json::Array(items) = crate::sql::json::parse(text, arena)? else {
                return Err(crate::sql::json::array_elements_error(tref.table, jsonb, kind));
            };
            let rows = arena.alloc_slice_with(items.len(), |_| EMPTY).map_err(|_| arena_full())?;
            for (slot, element) in rows.iter_mut().zip(items.iter()) {
                let datum = if as_text {
                    match *element {
                        crate::sql::json::Json::Str(s) => {
                            Datum::Text(crate::sql::json::decode_string(s, arena)?)
                        }
                        crate::sql::json::Json::Null => Datum::Null,
                        _ => Datum::Text(crate::sql::eval::json_to_text_pub(element, arena)?),
                    }
                } else {
                    Datum::Json { text: crate::sql::eval::json_to_text_pub(element, arena)?, jsonb }
                };
                *slot = crate::sql::exec::encode_projected_pub(&[datum], arena)?;
            }
            return Ok(&*rows);
        }
        // json: each element's verbatim source text.
        let items = crate::sql::json::array_elements_source(text, arena)?;
        let rows = arena.alloc_slice_with(items.len(), |_| EMPTY).map_err(|_| arena_full())?;
        for (slot, element) in rows.iter_mut().zip(items.iter()) {
            let datum = if as_text {
                match crate::sql::json::parse(element, arena)? {
                    crate::sql::json::Json::Str(s) => Datum::Text(crate::sql::json::decode_string(s, arena)?),
                    crate::sql::json::Json::Null => Datum::Null,
                    _ => Datum::Text(element),
                }
            } else {
                Datum::Json { text: element, jsonb }
            };
            *slot = crate::sql::exec::encode_projected_pub(&[datum], arena)?;
        }
        return Ok(&*rows);
    }
    // unnest(array): one row per element.
    if tref.table.eq_ignore_ascii_case("unnest") {
        let (element, raw) = match crate::sql::eval::eval(args[0], arena, params, &crate::sql::eval::NoColumns)? {
            Datum::Array { element, raw } => (element, raw),
            Datum::Null => return Ok(&[]),
            _ => return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "unnest requires an array argument")),
        };
        let count = crate::sql::array::len(raw);
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
        for (i, slot) in rows.iter_mut().enumerate() {
            let v = crate::sql::array::get(raw, element, i).unwrap_or(Datum::Null);
            *slot = crate::sql::exec::encode_projected_pub(&[v], arena)?;
        }
        return Ok(&*rows);
    }
    if args.len() != 2 && args.len() != 3 {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "generate_series expects 2 or 3 arguments"
        ));
    }
    // Temporal series: a date/timestamp start with an interval step.
    let start_val = crate::sql::eval::eval(args[0], arena, params, &crate::sql::eval::NoColumns)?;
    if let Some((base, kind)) = crate::sql::eval::timestamp_series_start(&start_val) {
        if args.len() != 3 {
            return Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "generate_series over timestamps requires a step"));
        }
        // Coerce bare string literals for the stop and step (function resolution).
        let stop_val = crate::sql::eval::cast_to(
            crate::sql::eval::eval(args[1], arena, params, &crate::sql::eval::NoColumns)?,
            kind.coltype(),
            arena,
        )?;
        let step_val = crate::sql::eval::cast_to(
            crate::sql::eval::eval(args[2], arena, params, &crate::sql::eval::NoColumns)?,
            ColType::Interval,
            arena,
        )?;
        let (Some((stop_micros, _)), Datum::Interval(step_iv)) =
            (crate::sql::eval::timestamp_series_start(&stop_val), step_val)
        else {
            return Ok(&[]);
        };
        let count = crate::sql::eval::timestamp_series_count(base, stop_micros, step_iv)?;
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
        let mut v = base;
        for slot in rows.iter_mut() {
            *slot = crate::sql::exec::encode_projected_pub(&[kind.datum(v)], arena)?;
            v = crate::sql::datetime::add_interval(v, step_iv);
        }
        return Ok(&*rows);
    }
    if start_val.is_null() {
        return Ok(&[]);
    }
    let as_i64 = |e: &'a Expr<'a>| -> Result<i64, SqlError> {
        match crate::sql::eval::eval(e, arena, params, &crate::sql::eval::NoColumns)? {
            Datum::Int4(v) => Ok(v as i64),
            Datum::Int8(v) => Ok(v),
            _ => Err(sql_err!(sqlstate::UNDEFINED_FUNCTION, "generate_series requires integer arguments")),
        }
    };
    let start = as_i64(args[0])?;
    let stop = as_i64(args[1])?;
    let step = if args.len() == 3 { as_i64(args[2])? } else { 1 };
    if step == 0 {
        return Err(sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
    }
    let count = if step > 0 {
        if stop < start { 0 } else { ((stop - start) / step) as usize + 1 }
    } else if stop > start {
        0
    } else {
        ((start - stop) / (-step)) as usize + 1
    };
    const EMPTY: &[u8] = &[];
    let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
    let mut v = start;
    for slot in rows.iter_mut() {
        *slot = crate::sql::exec::encode_projected_pub(&[Datum::Int8(v)], arena)?;
        v += step;
    }
    Ok(&*rows)
}
