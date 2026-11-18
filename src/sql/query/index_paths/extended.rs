//! The planner's use of the table's extended statistics
//! (`statext_clauselist_selectivity`, `estimate_multivariate_ndistinct`):
//! conditions on two or more keys of a statistics object are estimated from
//! its MCV list (`mcv_clauselist_selectivity`), then from its functional
//! dependencies (`dependencies_clauselist_selectivity`), and a grouping by
//! two or more of its keys counts its groups from its `ndistinct`.

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::eval::SqlError;
use crate::sql::types::{ArrElem, ColType, Datum};
use crate::storage::{ColumnSet, ExtendedStatisticsData, MAX_EXTENDED_STATISTICS_KEYS};

use super::Comparison;
use super::clauses::{Clause, Restriction};

/// One statistics object of the table over plain columns, its keys in
/// PostgreSQL's order.
#[derive(Clone, Copy)]
pub(crate) struct ExtendedObject<'a> {
    /// Each key's table column.
    pub(super) columns: [usize; MAX_EXTENDED_STATISTICS_KEYS],
    pub(super) n_keys: usize,
    pub(super) data: &'a ExtendedStatisticsData,
    /// Each MCV item's values, typed as their columns (`n_mcv * n_keys`).
    pub(super) mcv_values: &'a [Datum<'a>],
}

impl ExtendedObject<'_> {
    /// The columns among its keys.
    fn keys_mask(&self) -> ColumnSet {
        let mut keys = ColumnSet::EMPTY;
        for &column in self.columns[..self.n_keys]
            .iter()
            .filter(|&&column| column != usize::MAX)
        {
            keys.insert(column);
        }
        keys
    }

    fn key_of(&self, column: usize) -> Option<usize> {
        self.columns[..self.n_keys]
            .iter()
            .position(|&own| own == column)
    }
}

/// The table's statistics objects whose keys are all columns, with their
/// MCV values read as their columns' types; `Err` names a statistics object
/// the model does not read (one with expression keys whose expressions the
/// conditions compare, or MCV values of a type it cannot read).
pub(super) fn table_objects<'a>(
    storage: &crate::storage::Storage,
    txid: u32,
    slot: usize,
    definition: &crate::storage::TableDef,
    variables: &super::expressions::ExpressionVariables<'a>,
    arena: &'a Arena,
) -> Result<Result<&'a [ExtendedObject<'a>], &'static str>, SqlError> {
    let mut objects = crate::mem::arena::ArenaList::new(arena);
    for (statistics_slot, statistics) in storage.extended_statistics_for_table(slot, txid) {
        let data = storage.extended_statistics_data(statistics_slot, txid);
        if !data.valid || data.inherited {
            continue;
        }
        let order = crate::sql::catalog::extended_statistics_order(storage, &statistics, txid);
        let keys = statistics.keys_for(txid);
        let n_keys = usize::from(statistics.n_keys);
        let mut columns = [usize::MAX; MAX_EXTENDED_STATISTICS_KEYS];
        for (column, &position) in columns.iter_mut().zip(&order.positions[..n_keys]) {
            match &keys[usize::from(position)] {
                crate::storage::ExtendedStatisticsKey::Column(name) => {
                    *column = definition
                        .column_index(name.as_str())
                        .expect("a statistics column remains in its table");
                }
                // An expression key is the variable its conditions compare.
                crate::storage::ExtendedStatisticsKey::Expression(text) => {
                    if let Some(number) = variables.find_text(definition, text.as_str(), arena)? {
                        *column = number;
                    }
                }
            }
        }
        // An object with expression keys estimates conditions on those
        // expressions, which the model does not match; its columns' keys
        // are kept for conditions on them alone.
        let n_mcv = usize::from(data.n_mcv);
        let mcv_values = arena
            .alloc_slice_with(n_mcv * n_keys, |_| Datum::Null)
            .map_err(|_| crate::sql::eval::arena_full())?;
        for (item, entry) in data.mcv[..n_mcv].iter().enumerate() {
            let text = arena
                .alloc_str(entry.values.as_str())
                .map_err(|_| crate::sql::eval::arena_full())?;
            let parsed = crate::sql::array::parse_literal(text, ArrElem::Text, arena)?;
            for key in 0..n_keys {
                let value =
                    crate::sql::array::get(parsed, ArrElem::Text, key).ok_or_else(|| {
                        crate::sql_err!(
                            crate::sql::eval::sqlstate::INTERNAL_ERROR,
                            "corrupt extended-statistics MCV width"
                        )
                    })?;
                let column = columns[key];
                mcv_values[item * n_keys + key] = if value.is_null() || column == usize::MAX {
                    value
                } else {
                    let ctype = match variables.get(column) {
                        Some(variable) => variable.ctype,
                        None => definition.columns()[column].ctype,
                    };
                    if !matches!(
                        ctype,
                        ColType::Int2
                            | ColType::Int4
                            | ColType::Int8
                            | ColType::Float4
                            | ColType::Float8
                            | ColType::Numeric
                            | ColType::Text
                            | ColType::Varchar
                            | ColType::Bpchar
                            | ColType::Bool
                            | ColType::Date
                            | ColType::Timestamp
                            | ColType::Timestamptz
                            | ColType::Uuid
                            | ColType::Name
                            | ColType::Char
                            | ColType::Oid
                            | ColType::Time
                            | ColType::Timetz
                            | ColType::Interval
                            | ColType::Bytea
                            | ColType::Money
                            | ColType::Inet
                            | ColType::Cidr
                            | ColType::Macaddr
                            | ColType::Macaddr8
                            | ColType::Bit { .. }
                            | ColType::PgLsn
                    ) {
                        return Ok(Err("an MCV list of a type the model does not read"));
                    }
                    crate::sql::eval::cast_to(value, ctype, arena)?
                };
            }
        }
        let data = &*arena
            .alloc(data)
            .map_err(|_| crate::sql::eval::arena_full())?;
        objects
            .push(ExtendedObject {
                columns,
                n_keys,
                data,
                mcv_values,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
    }
    Ok(Ok(objects.as_slice()))
}

/// A condition as the statistics read it.
#[derive(Clone, Copy)]
pub(super) struct Condition<'c, 'a> {
    pub(super) clause: &'c Clause<'a>,
    pub(super) range: Option<(usize, bool)>,
    pub(super) selectivity: f64,
}

impl<'a> super::Planner<'_, 'a> {
    /// `clauselist_selectivity_ext` with extended statistics.
    pub(super) fn clauselist_ext(&self, conditions: &[Condition<'_, 'a>]) -> Result<f64, SqlError> {
        if conditions.len() <= 1 || self.extended.is_empty() || conditions.len() > 64 {
            return Ok(self.clauselist(
                conditions
                    .iter()
                    .map(|condition| (condition.range, condition.selectivity)),
            ));
        }
        let mut estimated = 0u64;
        let mut selectivity = self.mcv_selectivity(conditions, &mut estimated)?;
        selectivity *= self.dependencies_selectivity(conditions, &mut estimated);
        Ok(selectivity
            * self.clauselist(
                conditions
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| estimated & (1 << at) == 0)
                    .map(|(_, condition)| (condition.range, condition.selectivity)),
            ))
    }

    /// `clauselist_selectivity_or` of an `OR`'s arms: the arms a statistics
    /// object with an MCV list covers are estimated through it
    /// (`statext_mcv_clauselist_selectivity` with `is_or`), each object's
    /// result and every other arm then combined as independent.
    pub(super) fn or_selectivity(&self, arms: &[&[Restriction<'a>]]) -> Result<f64, SqlError> {
        let mut attnums = [ColumnSet::EMPTY; 64];
        if arms.len() > attnums.len() {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "an OR has more than 64 arms"
            ));
        }
        for (at, arm) in arms.iter().enumerate() {
            attnums[at] = arm
                .iter()
                .try_fold(ColumnSet::EMPTY, |mask, restriction| {
                    mcv_columns(&restriction.clause).map(|columns| mask.union(columns))
                })
                .unwrap_or(ColumnSet::EMPTY);
        }
        let mut estimated = 0u64;
        let mut selectivity = 0.0;
        while !self.extended.is_empty() {
            let mut best = None;
            let (mut best_matched, mut best_keys) = (2u32, MAX_EXTENDED_STATISTICS_KEYS + 1);
            for (position, object) in self.extended.iter().enumerate() {
                if object.data.n_mcv == 0 {
                    continue;
                }
                let keys = object.keys_mask();
                let matched = attnums[..arms.len()]
                    .iter()
                    .filter(|mask| !mask.is_empty() && mask.is_subset_of(keys))
                    .fold(ColumnSet::EMPTY, |all, &mask| all.union(mask))
                    .count_ones();
                if matched > best_matched || (matched == best_matched && object.n_keys < best_keys)
                {
                    best = Some(position);
                    best_matched = matched;
                    best_keys = object.n_keys;
                }
            }
            let Some(best) = best else {
                break;
            };
            let object = &self.extended[best];
            let keys = object.keys_mask();
            let n_items = usize::from(object.data.n_mcv);
            let mut or_matches = [false; 256];
            if n_items > or_matches.len() {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "an MCV list has more than 256 items"
                ));
            }
            let (mut simple_or, mut object_selectivity) = (0.0f64, 0.0f64);
            for (at, arm) in arms.iter().enumerate() {
                let mask = attnums[at];
                if mask.is_empty() || !mask.is_subset_of(keys) {
                    continue;
                }
                estimated |= 1 << at;
                attnums[at] = ColumnSet::EMPTY;
                let simple = self.simple_selectivity(arm)?;
                let overlap_simple = simple_or * simple;
                simple_or = (simple_or + simple - overlap_simple).clamp(0.0, 1.0);
                // `mcv_clause_selectivity_or`.
                let (mut matched, mut base, mut overlap_matched, mut overlap_base, mut total) =
                    (0.0, 0.0, 0.0, 0.0, 0.0);
                for (item, seen) in or_matches[..n_items].iter_mut().enumerate() {
                    let entry = &object.data.mcv[item];
                    let frequency = entry.count as f64 / object.data.rows as f64;
                    let base_frequency = f64::from_bits(entry.base_frequency_bits);
                    total += frequency;
                    let mut all = true;
                    for restriction in arm.iter() {
                        if !self.item_matches(object, item, &restriction.clause)? {
                            all = false;
                            break;
                        }
                    }
                    if all {
                        matched += frequency;
                        base += base_frequency;
                        if *seen {
                            overlap_matched += frequency;
                            overlap_base += base_frequency;
                        }
                    }
                    *seen |= all;
                }
                // A single-column arm keeps its simple estimate.
                let arm_selectivity = if mask.count_ones() == 1 {
                    simple
                } else {
                    combine(simple, matched, base, total)
                };
                let overlap = combine(overlap_simple, overlap_matched, overlap_base, total);
                object_selectivity =
                    (object_selectivity + arm_selectivity - overlap).clamp(0.0, 1.0);
            }
            selectivity = selectivity + object_selectivity - selectivity * object_selectivity;
        }
        for (at, arm) in arms.iter().enumerate() {
            if estimated & (1 << at) != 0 {
                continue;
            }
            let arm = self.selectivity(arm)?;
            selectivity = selectivity + arm - selectivity * arm;
        }
        Ok(selectivity)
    }

    /// `clauselist_selectivity` of `restrictions` without extended
    /// statistics.
    fn simple_selectivity(&self, restrictions: &[Restriction<'a>]) -> Result<f64, SqlError> {
        let mark = self.arena.mark();
        let result = conditions(restrictions, self.arena).map(|conditions| {
            self.clauselist(
                conditions
                    .iter()
                    .map(|condition| (condition.range, condition.selectivity)),
            )
        });
        // SAFETY: the conditions were consumed above.
        unsafe { self.arena.rewind_to(mark) };
        result
    }

    /// `statext_mcv_clauselist_selectivity` of an implicitly ANDed list.
    fn mcv_selectivity(
        &self,
        conditions: &[Condition<'_, 'a>],
        estimated: &mut u64,
    ) -> Result<f64, SqlError> {
        let mut attnums = [ColumnSet::EMPTY; 64];
        for (at, condition) in conditions.iter().enumerate() {
            attnums[at] = mcv_columns(condition.clause).unwrap_or(ColumnSet::EMPTY);
        }
        let mut selectivity = 1.0;
        loop {
            // `choose_best_statistics`: the most matched keys, then the
            // fewest keys, the first such object.
            let mut best = None;
            let (mut best_matched, mut best_keys) = (2u32, MAX_EXTENDED_STATISTICS_KEYS + 1);
            for (position, object) in self.extended.iter().enumerate() {
                if object.data.n_mcv == 0 {
                    continue;
                }
                let keys = object.keys_mask();
                let matched = attnums[..conditions.len()]
                    .iter()
                    .filter(|mask| !mask.is_empty() && mask.is_subset_of(keys))
                    .fold(ColumnSet::EMPTY, |all, &mask| all.union(mask))
                    .count_ones();
                if matched > best_matched || (matched == best_matched && object.n_keys < best_keys)
                {
                    best = Some(position);
                    best_matched = matched;
                    best_keys = object.n_keys;
                }
            }
            let Some(best) = best else {
                break;
            };
            let object = &self.extended[best];
            let keys = object.keys_mask();
            let mut chosen = [None::<Condition<'_, 'a>>; 64];
            let mut count = 0;
            for (at, condition) in conditions.iter().enumerate() {
                let mask = attnums[at];
                if mask.is_empty() || !mask.is_subset_of(keys) {
                    continue;
                }
                chosen[count] = Some(*condition);
                count += 1;
                *estimated |= 1 << at;
                attnums[at] = ColumnSet::EMPTY;
            }
            let chosen = &chosen[..count];
            let simple = self.clauselist(
                chosen
                    .iter()
                    .flatten()
                    .map(|condition| (condition.range, condition.selectivity)),
            );
            // `mcv_clauselist_selectivity`.
            let (mut matched, mut base, mut total) = (0.0, 0.0, 0.0);
            for item in 0..usize::from(object.data.n_mcv) {
                let entry = &object.data.mcv[item];
                let frequency = entry.count as f64 / object.data.rows as f64;
                total += frequency;
                let mut all = true;
                for condition in chosen.iter().flatten() {
                    if !self.item_matches(object, item, condition.clause)? {
                        all = false;
                        break;
                    }
                }
                if all {
                    base += f64::from_bits(entry.base_frequency_bits);
                    matched += frequency;
                }
            }
            // `mcv_combine_selectivities`.
            let mut other = (simple - base).clamp(0.0, 1.0);
            if other > 1.0 - total {
                other = 1.0 - total;
            }
            selectivity *= (matched + other).clamp(0.0, 1.0);
        }
        Ok(selectivity)
    }

    /// Whether MCV `item` of `object` satisfies `clause`
    /// (`mcv_get_match_bitmap`): a NULL value satisfies no operator.
    fn item_matches(
        &self,
        object: &ExtendedObject<'a>,
        item: usize,
        clause: &Clause<'_>,
    ) -> Result<bool, SqlError> {
        let value = |column: usize| {
            let key = object.key_of(column).expect("a matched clause reads a key");
            (key, &object.mcv_values[item * object.n_keys + key])
        };
        let apply = |column: usize, comparison: Comparison, constant: &Datum<'_>| {
            let (_, held) = value(column);
            if held.is_null() || constant.is_null() {
                return Ok(false);
            }
            let order = self.compare(self.column_collation(column), held, constant)?;
            Ok(comparison_holds(comparison, order))
        };
        match *clause {
            Clause::Compare {
                column,
                comparison,
                constant: Some(constant),
                ..
            } => apply(column, comparison, &constant),
            Clause::NullTest { column, is_null } => Ok(value(column).1.is_null() == is_null),
            Clause::Array {
                column,
                comparison,
                any,
                elements: Some(elements),
                ..
            } => {
                if value(column).1.is_null() {
                    return Ok(false);
                }
                let mut matched = !any;
                for element in elements {
                    let element_match = apply(column, comparison, element)?;
                    if any {
                        matched |= element_match;
                    } else {
                        matched &= element_match;
                    }
                }
                Ok(matched)
            }
            Clause::Or(arms) => {
                for arm in arms {
                    let mut all = true;
                    for restriction in *arm {
                        if !self.item_matches(object, item, &restriction.clause)? {
                            all = false;
                            break;
                        }
                    }
                    if all {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => unreachable!("only MCV-compatible clauses are matched"),
        }
    }

    /// `dependencies_clauselist_selectivity`.
    fn dependencies_selectivity(
        &self,
        conditions: &[Condition<'_, 'a>],
        estimated: &mut u64,
    ) -> f64 {
        let mut attnums = [usize::MAX; 64];
        let mut clause_columns = ColumnSet::EMPTY;
        for (at, condition) in conditions.iter().enumerate() {
            if *estimated & (1 << at) != 0 {
                continue;
            }
            if let Some(column) = dependency_column(condition.clause) {
                attnums[at] = column;
                clause_columns.insert(column);
            }
        }
        if clause_columns.count_ones() < 2 {
            return 1.0;
        }
        // The dependencies of the objects matching two or more columns.
        let usable = |object: &ExtendedObject<'_>| {
            object.data.dependencies_built
                && object.keys_mask().intersection(clause_columns).count_ones() >= 2
        };
        let mut chosen = [(0usize, 0usize, 0usize); 64];
        let mut chosen_count = 0;
        let mut remaining = clause_columns;
        loop {
            // `find_strongest_dependency`.
            let mut strongest: Option<(usize, usize, usize, f64)> = None;
            let available = remaining.count_ones() as usize;
            for (object_at, object) in self.extended.iter().enumerate() {
                if !usable(object) {
                    continue;
                }
                let mut position = 0usize;
                crate::storage::extended_statistics_dependency_order(
                    object.n_keys,
                    &mut |dependency| {
                        let at = position;
                        position += 1;
                        let degree = f64::from_bits(object.data.dependency_degree_bits[at]);
                        if degree == 0.0 || dependency.len() > available {
                            return;
                        }
                        if let Some((_, _, length, held)) = strongest
                            && (dependency.len() < length
                                || (length == dependency.len() && held > degree))
                        {
                            return;
                        }
                        if dependency.iter().all(|&key| {
                            let column = object.columns[usize::from(key)];
                            column != usize::MAX && remaining.contains(column)
                        }) {
                            strongest = Some((object_at, at, dependency.len(), degree));
                        }
                    },
                );
            }
            let Some((object_at, at, _, _)) = strongest else {
                break;
            };
            let object = &self.extended[object_at];
            let mut implied = usize::MAX;
            let mut position = 0usize;
            crate::storage::extended_statistics_dependency_order(
                object.n_keys,
                &mut |dependency| {
                    if position == at {
                        implied = object.columns[usize::from(dependency[dependency.len() - 1])];
                    }
                    position += 1;
                },
            );
            chosen[chosen_count] = (object_at, at, implied);
            chosen_count += 1;
            remaining = remaining.difference(ColumnSet::from_column(implied));
        }
        if chosen_count == 0 {
            return 1.0;
        }
        // `clauselist_apply_dependencies`: each column's own selectivity,
        // then each dependency, weakest last-found first.
        let mut columns = ColumnSet::EMPTY;
        for &(object_at, at, _) in &chosen[..chosen_count] {
            let object = &self.extended[object_at];
            let mut position = 0usize;
            crate::storage::extended_statistics_dependency_order(
                object.n_keys,
                &mut |dependency| {
                    if position == at {
                        for &key in dependency {
                            columns.insert(object.columns[usize::from(key)]);
                        }
                    }
                    position += 1;
                },
            );
        }
        let mut column_selectivity = [1.0f64; 64];
        let mut column_numbers = [0usize; 64];
        let mut count = 0;
        for column in columns.iter() {
            let mut matching = [None::<Condition<'_, 'a>>; 64];
            let mut matched = 0;
            for (at, condition) in conditions.iter().enumerate() {
                if attnums[at] == column {
                    matching[matched] = Some(*condition);
                    matched += 1;
                    *estimated |= 1 << at;
                }
            }
            column_selectivity[count] = self.clauselist(
                matching[..matched]
                    .iter()
                    .flatten()
                    .map(|condition| (condition.range, condition.selectivity)),
            );
            column_numbers[count] = column;
            count += 1;
        }
        let index_of = |column: usize| {
            column_numbers[..count]
                .iter()
                .position(|&own| own == column)
                .expect("a dependency's column has a selectivity")
        };
        for &(object_at, at, _) in chosen[..chosen_count].iter().rev() {
            let object = &self.extended[object_at];
            let degree = f64::from_bits(object.data.dependency_degree_bits[at]);
            let mut position = 0usize;
            let mut keys = [0usize; MAX_EXTENDED_STATISTICS_KEYS];
            let mut length = 0;
            crate::storage::extended_statistics_dependency_order(
                object.n_keys,
                &mut |dependency| {
                    if position == at {
                        for (slot, &key) in keys.iter_mut().zip(dependency) {
                            *slot = object.columns[usize::from(key)];
                        }
                        length = dependency.len();
                    }
                    position += 1;
                },
            );
            let mut determining = 1.0;
            for &column in &keys[..length - 1] {
                determining *= column_selectivity[index_of(column)];
            }
            let implied = index_of(keys[length - 1]);
            let s2 = column_selectivity[implied];
            column_selectivity[implied] = if determining <= s2 {
                degree + (1.0 - degree) * s2
            } else {
                degree * s2 / determining + (1.0 - degree) * s2
            };
        }
        column_selectivity[..count]
            .iter()
            .product::<f64>()
            .clamp(0.0, 1.0)
    }

    /// `estimate_multivariate_ndistinct` applied while `estimate_num_groups`
    /// groups `columns` of this table: the distinct counts of the column
    /// sets statistics objects cover, and the columns left.
    pub(super) fn multivariate_distinct(
        &self,
        columns: &mut [usize],
        count: &mut usize,
    ) -> MultivariateCounts {
        let mut found = MultivariateCounts::default();
        loop {
            let mut best: Option<(usize, u32)> = None;
            for (object_at, object) in self.extended.iter().enumerate() {
                if !object.data.ndistinct_built {
                    continue;
                }
                let shared = columns[..*count]
                    .iter()
                    .filter(|&&column| object.key_of(column).is_some())
                    .count() as u32;
                if shared < 2 {
                    continue;
                }
                if best.is_none_or(|(_, held)| shared > held) {
                    best = Some((object_at, shared));
                }
            }
            let Some((object_at, _)) = best else {
                break;
            };
            let object = &self.extended[object_at];
            let mut matched = 0u8;
            for &column in &columns[..*count] {
                if let Some(key) = object.key_of(column) {
                    matched |= 1 << key;
                }
            }
            let mut position = 0usize;
            let mut value = None;
            crate::storage::extended_statistics_combinations(object.n_keys, &mut |combination| {
                let mask = combination.iter().fold(0u8, |mask, &key| mask | 1 << key);
                if value.is_none() && mask == matched {
                    value = Some(object.data.ndistinct[position] as f64);
                }
                position += 1;
            });
            let value = value.expect("a combination covers the matched keys");
            found.push(value);
            let mut kept = 0;
            for at in 0..*count {
                if object.key_of(columns[at]).is_none() {
                    columns[kept] = columns[at];
                    kept += 1;
                }
            }
            *count = kept;
        }
        found
    }
}

/// The distinct counts `multivariate_distinct` found, at most one per
/// statistics object.
#[derive(Default)]
pub(super) struct MultivariateCounts {
    values: [f64; 32],
    len: usize,
}

impl MultivariateCounts {
    fn push(&mut self, value: f64) {
        self.values[self.len] = value;
        self.len += 1;
    }

    pub(super) fn values(&self) -> &[f64] {
        &self.values[..self.len]
    }
}

/// `mcv_combine_selectivities`.
fn combine(simple: f64, matched: f64, base: f64, total: f64) -> f64 {
    let other = (simple - base).clamp(0.0, 1.0).min(1.0 - total);
    (matched + other).clamp(0.0, 1.0)
}

fn comparison_holds(comparison: Comparison, order: Ordering) -> bool {
    match comparison {
        Comparison::Lt => order.is_lt(),
        Comparison::Le => order.is_le(),
        Comparison::Eq => order.is_eq(),
        Comparison::Ne => order.is_ne(),
        Comparison::Ge => order.is_ge(),
        Comparison::Gt => order.is_gt(),
    }
}

/// The table columns of a clause `statext_is_compatible_clause` accepts for
/// an MCV list, as a mask.
fn mcv_columns(clause: &Clause<'_>) -> Option<ColumnSet> {
    match *clause {
        Clause::Compare {
            column,
            constant: Some(_),
            pattern: false,
            ..
        }
        | Clause::NullTest { column, .. }
        | Clause::Array {
            column,
            elements: Some(_),
            ..
        } => Some(ColumnSet::from_column(column)),
        Clause::Or(arms) => {
            let mut mask = ColumnSet::EMPTY;
            for arm in arms {
                for restriction in *arm {
                    mask.union_with(mcv_columns(&restriction.clause)?);
                }
            }
            Some(mask)
        }
        _ => None,
    }
}

/// The column of a clause `dependency_is_compatible_clause` accepts: an
/// equality with a constant or an `= ANY` array, or an `OR` of such
/// clauses on one column.
fn dependency_column(clause: &Clause<'_>) -> Option<usize> {
    match *clause {
        Clause::Compare {
            column,
            comparison: Comparison::Eq,
            pattern: false,
            ..
        }
        | Clause::Array {
            column,
            comparison: Comparison::Eq,
            any: true,
            ..
        } => Some(column),
        Clause::Or(arms) => {
            let mut found = None;
            for arm in arms {
                let [restriction] = arm else {
                    return None;
                };
                let column = dependency_column(&restriction.clause)?;
                if found.is_some_and(|held| held != column) {
                    return None;
                }
                found = Some(column);
            }
            found
        }
        _ => None,
    }
}

/// The conditions of `restrictions`, for [`super::Planner::clauselist_ext`].
pub(super) fn conditions<'c, 'a>(
    restrictions: &'c [Restriction<'a>],
    arena: &'c Arena,
) -> Result<&'c [Condition<'c, 'a>], SqlError> {
    let out = arena
        .alloc_slice_with(restrictions.len(), |at| Condition {
            clause: &restrictions[at].clause,
            range: super::paths::range_bound(&restrictions[at].clause),
            selectivity: restrictions[at].selectivity,
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    Ok(out)
}
