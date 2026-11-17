//! The queries whose joins PostgreSQL's join search ([`super::joins`])
//! plans in pos3ql: a `SELECT` of plain columns from two to eight plain,
//! analyzed local tables without indexes, inner-joined by equalities of
//! their columns. EXPLAIN prints the plan the search chooses and execution
//! runs it ([`crate::sql::query::join_exec`]); other joins keep pos3ql's
//! own join order.

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, Expr, FromClause, JoinKind, Select, SelectItem, TableRef};
use crate::sql::eval::SqlError;
use crate::sql::types::{ColType, Datum};
use crate::storage::{Storage, TableStatistics};

use super::joins::{
    BaseRel, Clause, ClauseForm, EquivalenceClass, JoinPlan, JoinProblem, JoinSettings,
    JoinStatistics, MAX_JOIN_RELS, MAX_MEMBERS, Member, NeededColumn, Relids,
};
use super::{Comparison, Costs, Planned, Planner, QueryShape, ScanPath, clamp_probability};
use crate::sql::query::scope::{QueryScope, ResolvedColumn};

/// One base relation of a modeled join.
#[derive(Clone, Copy)]
pub(crate) struct ModeledRel<'a> {
    /// The FROM item, read alone.
    pub(crate) from: &'a FromClause<'a>,
    /// Its own conditions, which its scan filters by.
    pub(crate) restriction: Option<&'a Expr<'a>>,
    pub(crate) path: ScanPath<'a>,
}

/// The plan of a modeled join and what its nodes read.
pub(crate) struct ModeledJoin<'a> {
    pub(crate) plan: JoinPlan<'a>,
    pub(crate) rels: &'a [ModeledRel<'a>],
    /// Each class member's FROM position and column.
    pub(crate) columns: &'a [[(usize, usize); MAX_MEMBERS]],
}

impl ModeledJoin<'_> {
    /// The FROM position and column of member `member` of class `class`.
    pub(crate) fn member(&self, class: u8, member: u8) -> (usize, usize) {
        self.columns[usize::from(class)][usize::from(member)]
    }

    /// The `(left, right)` columns a join clause equates, as written.
    pub(crate) fn clause_columns(&self, clause: &Clause) -> ((usize, usize), (usize, usize)) {
        match clause.form {
            ClauseForm::Equality { class, left, right } => {
                (self.member(class, left), self.member(class, right))
            }
            ClauseForm::Other => unreachable!("modeled joins equate columns"),
        }
    }
}

/// The column types whose equality the model hashes and merges.
fn joinable(ctype: ColType) -> bool {
    matches!(
        ctype,
        ColType::Int2
            | ColType::Int4
            | ColType::Int8
            | ColType::Text
            | ColType::Numeric
            | ColType::Date
            | ColType::Bool
    )
}

/// The plan PostgreSQL's join search chooses for `statement`, when the
/// query is inside the model; `None` otherwise.
pub(crate) fn modeled_join<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Option<ModeledJoin<'a>>, SqlError> {
    let Some(from) = statement.from.as_ref() else {
        return Ok(None);
    };
    let n = scope.n;
    if !(2..=MAX_JOIN_RELS).contains(&n)
        || n != from.joins.len() + 1
        || statement.set_body.is_some()
        || !statement.with.is_empty()
        || statement.distinct
        || !statement.distinct_on.is_empty()
        || !statement.group_by.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.having.is_some()
        || !statement.order_by.is_empty()
        || statement.limit.is_some()
        || statement.offset.is_some()
        || !statement.windows.is_empty()
        || !statement.locking.is_empty()
        || scope.n_merged != 0
    {
        return Ok(None);
    }
    let methods = crate::sql::guc::planner_methods();
    let value = crate::sql::guc::planner_value;
    // Above `geqo_threshold` PostgreSQL searches genetically, and past the
    // collapse limits it plans several join problems.
    if (value("geqo") != 0.0 && n as f64 >= value("geqo_threshold"))
        || n as f64 > value("join_collapse_limit")
        || n as f64 > value("from_collapse_limit")
    {
        return Ok(None);
    }
    for join in from.joins {
        if !matches!(
            join.kind,
            JoinKind::Inner | JoinKind::Cross | JoinKind::Comma
        ) || join.using.is_some()
            || join.natural
        {
            return Ok(None);
        }
    }
    for table in 0..n {
        let tref = source(from, table);
        if !plain_table(storage, txid, scope, table, tref) {
            return Ok(None);
        }
    }
    // The output: plain columns and wildcards.
    let mut output = [0u64; MAX_JOIN_RELS];
    // Each relation's columns in the order its target list takes them:
    // the output's first, then the join clauses' (`build_base_rel_tlists`,
    // then `distribute_qual_to_rels`).
    let mut appearance = [[0u8; 64]; MAX_JOIN_RELS];
    let mut appeared = [0usize; MAX_JOIN_RELS];
    let mut note = |table: usize, column: usize| {
        if column < 64 && !appearance[table][..appeared[table]].contains(&(column as u8)) {
            appearance[table][appeared[table]] = column as u8;
            appeared[table] += 1;
        }
    };
    let width = |table: usize| {
        scope
            .defs
            .stored(table)
            .map_or(0, |def| def.columns().len().min(64))
    };
    for item in statement.items {
        match item {
            SelectItem::Wildcard(_) => {
                for (table, mask) in output.iter_mut().enumerate().take(n) {
                    *mask = u64::MAX;
                    (0..width(table)).for_each(|column| note(table, column));
                }
            }
            SelectItem::TableWildcard(qualifier) => {
                let Some(table) = (0..n).find(|&table| scope.names[table] == *qualifier) else {
                    return Ok(None);
                };
                output[table] = u64::MAX;
                (0..width(table)).for_each(|column| note(table, column));
            }
            SelectItem::Expr { expression, .. } => match column_of(scope, expression) {
                Some((table, column)) if column < 64 => {
                    output[table] |= 1 << column;
                    note(table, column);
                }
                _ => return Ok(None),
            },
            SelectItem::RecordStar(_) => return Ok(None),
        }
    }
    // The conditions, in the order PostgreSQL distributes them: each join's
    // `ON`, then `WHERE`.
    let mut conjuncts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
    let mut count = 0;
    for condition in from
        .joins
        .iter()
        .filter_map(|join| join.on)
        .chain(statement.where_clause)
    {
        if !super::super::plan::flatten_and(condition, &mut conjuncts, &mut count) {
            return Ok(None);
        }
    }
    let mut restrictions = [None::<&'a Expr<'a>>; MAX_JOIN_RELS];
    let mut classes: [EquivalenceClass; MAX_JOIN_RELS * 2] = [EquivalenceClass {
        members: [Member { rel: 0, column: 0 }; MAX_MEMBERS],
        count: 0,
        has_const: false,
    }; MAX_JOIN_RELS * 2];
    let mut class_count = 0usize;
    let mut sources = [(
        0u8,
        Member { rel: 0, column: 0 },
        Member { rel: 0, column: 0 },
    ); 32];
    let mut source_count = 0usize;
    let mut join_columns = [0u64; MAX_JOIN_RELS];
    for &conjunct in &conjuncts[..count] {
        let Some(mask) = super::super::plan::expr_tables(conjunct, scope) else {
            return Ok(None);
        };
        if mask.count_ones() == 1 {
            let table = mask.trailing_zeros() as usize;
            restrictions[table] = Some(match restrictions[table] {
                None => conjunct,
                Some(held) => and(held, conjunct, arena)?,
            });
            continue;
        }
        let Expr::Binary {
            operator: BinaryOp::Eq,
            left,
            right,
            ..
        } = conjunct
        else {
            return Ok(None);
        };
        let (Some(left), Some(right)) = (column_of(scope, left), column_of(scope, right)) else {
            return Ok(None);
        };
        if left.0 == right.0 || left.1 >= 64 || right.1 >= 64 || source_count == sources.len() {
            return Ok(None);
        }
        let left_meta = &scope.defs.stored(left.0).expect("plain table").columns()[left.1];
        let right_meta = &scope.defs.stored(right.0).expect("plain table").columns()[right.1];
        if left_meta.ctype != right_meta.ctype
            || !joinable(left_meta.ctype)
            || left_meta.collation != right_meta.collation
        {
            return Ok(None);
        }
        join_columns[left.0] |= 1 << left.1;
        join_columns[right.0] |= 1 << right.1;
        note(left.0, left.1);
        note(right.0, right.1);
        let left = Member {
            rel: left.0 as u8,
            column: left.1 as u16,
        };
        let right = Member {
            rel: right.0 as u8,
            column: right.1 as u16,
        };
        // `process_equivalence`: a new class, a member added to one, or two
        // classes merged into the left one's.
        let find = |classes: &[EquivalenceClass], member: Member| {
            classes
                .iter()
                .position(|class| class.members().contains(&member))
        };
        let class = match (
            find(&classes[..class_count], left),
            find(&classes[..class_count], right),
        ) {
            (Some(l), Some(r)) if l == r => l,
            (Some(l), Some(r)) => {
                let moved = classes[r];
                for &member in moved.members() {
                    push_member(&mut classes[l], member)?;
                }
                // Clauses of the merged class now name the left one.
                for entry in &mut sources[..source_count] {
                    if usize::from(entry.0) == r {
                        entry.0 = l as u8;
                    } else if usize::from(entry.0) > r {
                        entry.0 -= 1;
                    }
                }
                classes.copy_within(r + 1..class_count, r);
                class_count -= 1;
                if l > r { l - 1 } else { l }
            }
            (Some(l), None) => {
                push_member(&mut classes[l], right)?;
                l
            }
            (None, Some(r)) => {
                push_member(&mut classes[r], left)?;
                r
            }
            (None, None) => {
                if class_count == classes.len() {
                    return Ok(None);
                }
                classes[class_count].count = 0;
                push_member(&mut classes[class_count], left)?;
                push_member(&mut classes[class_count], right)?;
                class_count += 1;
                class_count - 1
            }
        };
        sources[source_count] = (class as u8, left, right);
        source_count += 1;
    }
    // A class member compared with a constant gives every member that
    // restriction (a class with a constant), which the model leaves out.
    for restriction in restrictions.iter().flatten() {
        let mut parts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
        let mut parts_count = 0;
        super::super::plan::flatten_and(restriction, &mut parts, &mut parts_count);
        for part in &parts[..parts_count] {
            if let Expr::Binary {
                operator: BinaryOp::Eq,
                left,
                right,
                ..
            } = part
            {
                for side in [left, right] {
                    if let Some((table, column)) = column_of(scope, side)
                        && classes[..class_count].iter().any(|class| {
                            class.members().contains(&Member {
                                rel: table as u8,
                                column: column as u16,
                            })
                        })
                    {
                        return Ok(None);
                    }
                }
            }
        }
    }
    let costs = Costs::current();
    let mut clauses = [Clause {
        form: ClauseForm::Other,
        relids: 0,
        cost: costs.cpu_operator,
        selectivity: None,
        source: None,
    }; 32];
    for (at, &(class, left, right)) in sources[..source_count].iter().enumerate() {
        let members = classes[usize::from(class)].members();
        let position = |member: Member| members.iter().position(|held| *held == member);
        clauses[at] = Clause {
            form: ClauseForm::Equality {
                class,
                left: position(left).expect("member") as u8,
                right: position(right).expect("member") as u8,
            },
            relids: 1 << left.rel | 1 << right.rel,
            cost: costs.cpu_operator,
            selectivity: None,
            source: Some(at as u16),
        };
    }
    // Each relation's scan: PostgreSQL's cheapest path under its own
    // conditions, returning the columns joins and the output read.
    let mut rels = [None::<ModeledRel<'a>>; MAX_JOIN_RELS];
    let mut base = [BaseRel {
        rows: 0.0,
        width: 0.0,
        startup: 0.0,
        total: 0.0,
    }; MAX_JOIN_RELS];
    let mut needed = [NeededColumn {
        rel: 0,
        column: 0,
        width: 0.0,
        needed_by: 0,
        output: false,
    }; 64 * 2];
    let mut needed_count = 0;
    for table in 0..n {
        let single = arena
            .alloc(FromClause {
                base: *source(from, table),
                joins: &[],
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        let single_scope = QueryScope::resolve_schema(storage, single, txid, arena, None)?;
        let definition = scope.defs.stored(table).expect("plain table");
        let columns = definition.columns().len();
        let all = if columns >= 64 {
            u64::MAX
        } else {
            (1u64 << columns) - 1
        };
        let above = (output[table] | join_columns[table]) & all;
        let shape = QueryShape {
            output: Some(above),
            ..QueryShape::default()
        };
        let planned = super::scan_path(
            storage,
            &single_scope,
            txid,
            restrictions[table],
            None,
            params,
            arena,
            shape,
        )?;
        let Planned::Chosen(path) = planned else {
            return Ok(None);
        };
        if path.kind != super::ScanKind::Seq {
            return Ok(None);
        }
        let slot = scope.slots[table];
        for column in appearance[table][..appeared[table]]
            .iter()
            .map(|&column| usize::from(column))
            .filter(|&column| above & 1 << column != 0)
        {
            if needed_count == needed.len() {
                return Ok(None);
            }
            let needed_by = classes[..class_count]
                .iter()
                .filter(|class| {
                    class.members().contains(&Member {
                        rel: table as u8,
                        column: column as u16,
                    })
                })
                .fold(0 as Relids, |relids, class| {
                    relids
                        | class
                            .members()
                            .iter()
                            .fold(0, |relids, member| relids | 1 << member.rel)
                });
            needed[needed_count] = NeededColumn {
                rel: table as u8,
                column: column as u16,
                width: f64::from(super::column_width(storage, txid, slot, column)),
                needed_by: needed_by & !(1 << table),
                output: output[table] & 1 << column != 0,
            };
            needed_count += 1;
        }
        base[table] = BaseRel {
            rows: path.rows,
            width: f64::from(path.width),
            startup: path.startup_cost,
            total: path.total_cost,
        };
        rels[table] = Some(ModeledRel {
            from: single,
            restriction: restrictions[table],
            path,
        });
    }
    // The statistics the selectivities read.
    let statistics: [Option<TableStatistics>; MAX_JOIN_RELS] = core::array::from_fn(|table| {
        (table < n).then(|| storage.table_statistics(scope.slots[table], txid))
    });
    let planners: [Option<Planner<'_, 'a>>; MAX_JOIN_RELS] = core::array::from_fn(|table| {
        let statistics = statistics[table].as_ref()?;
        let slot = scope.slots[table];
        let definition = scope.defs.stored(table)?;
        Some(Planner {
            variables: super::ExpressionVariables {
                first: definition.columns().len(),
                variables: &[],
            },
            storage,
            txid,
            slot,
            definition,
            statistics,
            statistics_key: 0,
            size: super::relation_size(storage, slot, txid),
            costs,
            indexes: &[],
            extended: &[],
            arena,
        })
    });
    let reader = RelStatistics {
        planners: &planners,
        rels: &base,
    };
    let problem = JoinProblem {
        rels: &base[..n],
        classes: &classes[..class_count],
        clauses: &clauses[..source_count],
        needed: &needed[..needed_count],
        settings: JoinSettings {
            costs,
            work_mem_bytes: f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0,
            hash_mem_bytes: f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0 * 2.0,
            nestloop: methods.nestloop,
            hashjoin: methods.hashjoin,
            mergejoin: value("enable_mergejoin") != 0.0,
            material: methods.material,
            sort: methods.sort,
        },
    };
    let Ok(plan) = super::joins::search(&problem, &reader, arena)? else {
        return Ok(None);
    };
    // A hash join planned with several batches assigns rows by their keys'
    // hash values, which only some key types have here.
    for path in plan.paths {
        if let super::joins::PathKind::HashJoin { hash, batches, .. } = path.kind
            && batches > 1
            && hash.as_slice().iter().any(|&id| {
                let ClauseForm::Equality { class, left, .. } = plan.clauses[usize::from(id)].form
                else {
                    return true;
                };
                let member = classes[usize::from(class)].members()[usize::from(left)];
                let ctype = scope
                    .defs
                    .stored(usize::from(member.rel))
                    .expect("plain table")
                    .columns()[usize::from(member.column)]
                .ctype;
                !matches!(
                    ctype,
                    ColType::Int2
                        | ColType::Int4
                        | ColType::Int8
                        | ColType::Date
                        | ColType::Bool
                        | ColType::Text
                )
            })
        {
            return Ok(None);
        }
    }
    let rels = arena
        .alloc_slice_with(n, |table| rels[table].expect("planned"))
        .map_err(|_| crate::sql::eval::arena_full())?;
    let columns = arena
        .alloc_slice_with(class_count, |class| {
            let mut columns = [(0usize, 0usize); MAX_MEMBERS];
            for (at, member) in classes[class].members().iter().enumerate() {
                columns[at] = (usize::from(member.rel), usize::from(member.column));
            }
            columns
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    Ok(Some(ModeledJoin {
        plan,
        rels,
        columns,
    }))
}

fn push_member(class: &mut EquivalenceClass, member: Member) -> Result<(), SqlError> {
    if usize::from(class.count) == MAX_MEMBERS {
        return Err(crate::sql_err!(
            crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
            "an equivalence class of more than {} columns is not supported",
            MAX_MEMBERS
        ));
    }
    class.members[usize::from(class.count)] = member;
    class.count += 1;
    Ok(())
}

fn and<'a>(
    left: &'a Expr<'a>,
    right: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    arena
        .alloc(Expr::Binary {
            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            operator: BinaryOp::And,
            left,
            right,
        })
        .map(|expression| &*expression)
        .map_err(|_| crate::sql::eval::arena_full())
}

pub(crate) fn source<'a>(from: &'a FromClause<'a>, table: usize) -> &'a TableRef<'a> {
    if table == 0 {
        &from.base
    } else {
        &from.joins[table - 1].table
    }
}

/// The FROM position and column a plain column reference reads.
fn column_of(scope: &QueryScope<'_>, expression: &Expr<'_>) -> Option<(usize, usize)> {
    let Expr::Column { qualifier, name } = expression else {
        return None;
    };
    match scope.find_column(*qualifier, name).ok()? {
        ResolvedColumn::Table(table, column) => Some((table, column)),
        _ => None,
    }
}

/// A plain local table without descendants, indexes, row security or
/// sampling, read under its own name.
fn plain_table(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    table: usize,
    tref: &TableRef<'_>,
) -> bool {
    let slot = scope.slots[table];
    if slot == usize::MAX
        || scope.derived[table].is_some()
        || scope.lateral[table]
        || tref.subquery.is_some()
        || tref.func_args.is_some()
        || tref.rows_from.is_some()
        || tref.col_alias.is_some()
        || tref.sample.is_some()
        || tref.cte.is_some()
    {
        return false;
    }
    let Some(definition) = scope.defs.stored(table) else {
        return false;
    };
    definition.kind == crate::storage::TableKind::Local
        && !definition.partition.is_partitioned()
        && !storage.relation_has_descendants(slot, txid)
        && !super::has_indexes(storage, txid, slot)
        && storage.table_statistics(slot, txid).valid
        && !definition.row_level_security.enabled
}

/// The statistics of a modeled join's relations.
struct RelStatistics<'r, 's, 'a> {
    planners: &'r [Option<Planner<'s, 'a>>; MAX_JOIN_RELS],
    rels: &'r [BaseRel; MAX_JOIN_RELS],
}

impl RelStatistics<'_, '_, '_> {
    fn planner(&self, member: Member) -> &Planner<'_, '_> {
        self.planners[usize::from(member.rel)]
            .as_ref()
            .expect("a member's relation has statistics")
    }

    /// `get_variable_range` of an analyzed column without an index: the
    /// least and greatest of its histogram bounds and common values.
    fn range<'v>(
        &self,
        planner: &'v Planner<'_, '_>,
        column: usize,
    ) -> Result<Option<(Datum<'v>, Datum<'v>)>, SqlError> {
        let variable = planner.variable(column);
        if !variable.analyzed {
            return Ok(None);
        }
        let Some(distribution) = variable.distribution else {
            return Ok(None);
        };
        let mut range: Option<(Datum<'v>, Datum<'v>)> = None;
        let histogram = distribution.histogram_len();
        if histogram > 0 {
            range = Some((
                distribution.histogram_value(0, variable.ctype)?,
                distribution.histogram_value(histogram - 1, variable.ctype)?,
            ));
        }
        for index in 0..distribution.mcv_len() {
            let value = distribution.mcv_value(index, variable.ctype)?;
            range = Some(match range {
                None => (value, value),
                Some((low, high)) => (
                    if planner.compare(variable.collation, &value, &low)?.is_lt() {
                        value
                    } else {
                        low
                    },
                    if planner.compare(variable.collation, &value, &high)?.is_gt() {
                        value
                    } else {
                        high
                    },
                ),
            });
        }
        Ok(range)
    }
}

impl JoinStatistics for RelStatistics<'_, '_, '_> {
    fn equality(&self, left: Member, right: Member) -> Result<f64, SqlError> {
        let (first, second) = (self.planner(left), self.planner(right));
        let one = first.variable(usize::from(left.column));
        let two = second.variable(usize::from(right.column));
        let (nd1, _) = first.distinct(&one);
        let (nd2, _) = second.distinct(&two);
        let mcv1 = one
            .distribution
            .filter(|distribution| one.analyzed && distribution.mcv_len() > 0);
        let mcv2 = two
            .distribution
            .filter(|distribution| two.analyzed && distribution.mcv_len() > 0);
        let (Some(mcv1), Some(mcv2)) = (mcv1, mcv2) else {
            let null1 = if one.analyzed { one.null_fraction } else { 0.0 };
            let null2 = if two.analyzed { two.null_fraction } else { 0.0 };
            return Ok((1.0 - null1) * (1.0 - null2) / nd1.max(nd2));
        };
        let (count1, count2) = (mcv1.mcv_len(), mcv2.mcv_len());
        let mut matched2 = [false; 10_000];
        if count2 > matched2.len() {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a join of columns with more than 10000 most common values is not supported"
            ));
        }
        let mut matched1_freq = 0.0;
        let mut unmatched1 = 0.0;
        let mut product = 0.0;
        let mut matches = 0usize;
        for i in 0..count1 {
            let value = mcv1.mcv_value(i, one.ctype)?;
            let frequency = f64::from(mcv1.mcv_frequency(i));
            let mut hit = false;
            for (j, matched) in matched2.iter_mut().enumerate().take(count2) {
                if *matched {
                    continue;
                }
                let other = mcv2.mcv_value(j, two.ctype)?;
                if first.compare(one.collation, &value, &other)?.is_eq() {
                    *matched = true;
                    hit = true;
                    product += frequency * f64::from(mcv2.mcv_frequency(j));
                    matches += 1;
                    break;
                }
            }
            if hit {
                matched1_freq += frequency;
            } else {
                unmatched1 += frequency;
            }
        }
        let product = clamp_probability(product);
        let matched1 = clamp_probability(matched1_freq);
        let unmatched1 = clamp_probability(unmatched1);
        let (mut matched2_freq, mut unmatched2) = (0.0, 0.0);
        for (j, matched) in matched2.iter().enumerate().take(count2) {
            let frequency = f64::from(mcv2.mcv_frequency(j));
            if *matched {
                matched2_freq += frequency;
            } else {
                unmatched2 += frequency;
            }
        }
        let matched2 = clamp_probability(matched2_freq);
        let unmatched2 = clamp_probability(unmatched2);
        let other1 = clamp_probability(1.0 - one.null_fraction - matched1 - unmatched1);
        let other2 = clamp_probability(1.0 - two.null_fraction - matched2 - unmatched2);
        let mut total1 = product;
        if nd2 > count2 as f64 {
            total1 += unmatched1 * other2 / (nd2 - count2 as f64);
        }
        if nd2 > matches as f64 {
            total1 += other1 * (other2 + unmatched2) / (nd2 - matches as f64);
        }
        let mut total2 = product;
        if nd1 > count1 as f64 {
            total2 += unmatched2 * other1 / (nd1 - count1 as f64);
        }
        if nd1 > matches as f64 {
            total2 += other2 * (other1 + unmatched1) / (nd1 - matches as f64);
        }
        Ok(total1.min(total2))
    }

    fn bucket(&self, member: Member, buckets: f64) -> Result<(f64, f64), SqlError> {
        let planner = self.planner(member);
        let variable = planner.variable(usize::from(member.column));
        let mcv_frequency = variable
            .distribution
            .filter(|distribution| variable.analyzed && distribution.mcv_len() > 0)
            .map_or(0.0, |distribution| f64::from(distribution.mcv_frequency(0)));
        let (mut distinct, default) = planner.distinct(&variable);
        if default {
            return Ok((mcv_frequency, mcv_frequency.max(0.1)));
        }
        let null_fraction = if variable.analyzed {
            variable.null_fraction
        } else {
            0.0
        };
        let average = (1.0 - null_fraction) / distinct;
        let tuples = planner.size.tuples;
        if tuples > 0.0 {
            distinct =
                super::clamp_row_est(distinct * self.rels[usize::from(member.rel)].rows / tuples);
        }
        let mut fraction = if distinct > buckets {
            1.0 / buckets
        } else {
            1.0 / distinct
        };
        if average > 0.0 && mcv_frequency > average {
            fraction *= mcv_frequency / average;
        }
        Ok((mcv_frequency, fraction.clamp(1.0e-6, 1.0)))
    }

    fn merge_range(&self, left: Member, right: Member) -> Result<Option<[f64; 4]>, SqlError> {
        let (first, second) = (self.planner(left), self.planner(right));
        let (left_column, right_column) = (usize::from(left.column), usize::from(right.column));
        let mut range = [0.0, 1.0, 0.0, 1.0];
        let (Some((left_min, left_max)), Some((right_min, right_max))) = (
            self.range(first, left_column)?,
            self.range(second, right_column)?,
        ) else {
            return Ok(Some(range));
        };
        let default = super::DEFAULT_INEQ_SEL;
        let selectivity = |planner: &Planner<'_, '_>, column, comparison, value| {
            planner.inequality_selectivity(column, comparison, value)
        };
        let Ok(left_end) = selectivity(first, left_column, Comparison::Le, right_max)? else {
            return Ok(None);
        };
        let Ok(right_end) = selectivity(second, right_column, Comparison::Le, left_max)? else {
            return Ok(None);
        };
        if left_end != default {
            range[1] = left_end;
        }
        if right_end != default {
            range[3] = right_end;
        }
        if range[1] > range[3] {
            range[1] = 1.0;
        } else if range[1] < range[3] {
            range[3] = 1.0;
        } else {
            range[1] = 1.0;
            range[3] = 1.0;
        }
        let Ok(left_start) = selectivity(first, left_column, Comparison::Lt, right_min)? else {
            return Ok(None);
        };
        let Ok(right_start) = selectivity(second, right_column, Comparison::Lt, left_min)? else {
            return Ok(None);
        };
        if left_start != default {
            range[0] = left_start;
        }
        if right_start != default {
            range[2] = right_start;
        }
        if range[0] < range[2] {
            range[0] = 0.0;
        } else if range[0] > range[2] {
            range[2] = 0.0;
        } else {
            range[0] = 0.0;
            range[2] = 0.0;
        }
        if range[0] >= range[1] {
            range[0] = 0.0;
            range[1] = 1.0;
        }
        if range[2] >= range[3] {
            range[2] = 0.0;
            range[3] = 1.0;
        }
        Ok(Some(range))
    }
}
