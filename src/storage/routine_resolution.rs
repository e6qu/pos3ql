//! Typed overload selection across stored and virtual catalog namespaces.

use super::{
    MAX_ROUTINE_ARGUMENTS, PolymorphicBinding, PolymorphicType, RoutineCallKind,
    RoutineCallSelection, RoutineInputSignature, RoutineParameterMode, SqlName, Storage,
    current_database,
};
use crate::sql::catalog::{BuiltinCallableSignature, builtin_callable_signatures};
use crate::sql::types::{ColType, oid, type_category};

struct Candidate {
    selected: RoutineCallSelection,
    rank: usize,
    argument_oids: [i32; MAX_ROUTINE_ARGUMENTS],
    argument_names: [SqlName; MAX_ROUTINE_ARGUMENTS],
    defaulted: [bool; MAX_ROUTINE_ARGUMENTS],
    count: usize,
    variadic_element: Option<i32>,
    is_procedure: bool,
}

pub(super) fn map_parameter_positions<'parameter>(
    formal_count: usize,
    variadic_index: Option<usize>,
    parameter: impl Fn(usize) -> Option<(&'parameter str, bool)>,
    names: &[Option<&str>],
    count: usize,
    explicit_variadic: bool,
) -> Option<[u8; MAX_ROUTINE_ARGUMENTS]> {
    if count > MAX_ROUTINE_ARGUMENTS || (!names.is_empty() && names.len() != count) {
        return None;
    }
    // VARIADIC disables tail expansion; fixed signatures also accept it.
    let variadic = variadic_index;
    if formal_count > MAX_ROUTINE_ARGUMENTS
        || !explicit_variadic && variadic.is_some() && names.iter().any(Option::is_some)
    {
        return None;
    }
    let mut mapping = [u8::MAX; MAX_ROUTINE_ARGUMENTS];
    let mut supplied = [false; MAX_ROUTINE_ARGUMENTS];
    let mut named = false;
    for (actual, target) in mapping.iter_mut().take(count).enumerate() {
        let name = names.get(actual).copied().flatten();
        let input = if let Some(name) = name {
            named = true;
            let input = (0..formal_count)
                .find(|index| parameter(*index).is_some_and(|(formal, _)| formal == name))?;
            if variadic == Some(input) && !explicit_variadic {
                return None;
            }
            input
        } else {
            if named {
                return None;
            }
            if actual < formal_count {
                actual
            } else if !explicit_variadic {
                variadic?
            } else {
                return None;
            }
        };
        let expanded_tail = !explicit_variadic && !named && variadic == Some(input);
        if supplied[input] && !expanded_tail {
            return None;
        }
        supplied[input] = true;
        *target = input as u8;
    }
    if explicit_variadic
        && (count == 0 || variadic.is_some_and(|input| usize::from(mapping[count - 1]) != input))
    {
        return None;
    }
    for (input, supplied) in supplied.iter().enumerate().take(formal_count) {
        if !supplied && !parameter(input)?.1 {
            return None;
        }
    }
    Some(mapping)
}

impl Candidate {
    fn stored(
        storage: &Storage,
        transaction: u32,
        slot: usize,
        rank: usize,
        signature: &RoutineInputSignature,
        include_outputs: bool,
    ) -> Option<Self> {
        let mut candidate = Self {
            selected: RoutineCallSelection::Stored(slot),
            rank,
            argument_oids: [0; MAX_ROUTINE_ARGUMENTS],
            argument_names: [SqlName::EMPTY; MAX_ROUTINE_ARGUMENTS],
            defaulted: [false; MAX_ROUTINE_ARGUMENTS],
            count: if include_outputs {
                signature.parameter_count
            } else {
                signature.argument_count
            },
            variadic_element: None,
            is_procedure: signature.is_procedure,
        };
        for index in 0..candidate.count {
            let parameter = if include_outputs {
                &signature.parameters[index]
            } else {
                signature.parameter_for_input(index)?
            };
            let argument = super::RoutineArgumentDef {
                name: parameter.name,
                ctype: parameter.ctype,
                user_type: parameter.user_type,
            };
            candidate.argument_oids[index] =
                storage.routine_argument_oid(&argument, transaction)?;
            candidate.argument_names[index] = parameter.name;
            candidate.defaulted[index] = parameter.mode.has_default();
            if matches!(parameter.mode, RoutineParameterMode::Variadic { .. }) {
                candidate.variadic_element = Some(match argument.polymorphic_type() {
                    Some(PolymorphicType::Array) => oid::ANYELEMENT,
                    Some(PolymorphicType::CompatibleArray) => oid::ANYCOMPATIBLE,
                    _ => match argument.ctype {
                        ColType::Array(element) => element.element_oid(),
                        _ => return None,
                    },
                });
            }
        }
        Some(candidate)
    }

    fn builtin(rank: usize, signature: &BuiltinCallableSignature, include_outputs: bool) -> Self {
        let include_outputs = include_outputs && !signature.catalog.all_arguments.is_empty();
        let arguments = if include_outputs {
            signature.catalog.all_arguments
        } else {
            signature.arguments
        };
        let names = if include_outputs {
            signature.catalog.all_argument_names
        } else {
            signature.argument_names
        };
        let mut candidate = Self {
            selected: RoutineCallSelection::Builtin {
                oid: signature.oid,
                schema: signature.schema,
            },
            rank,
            argument_oids: [0; MAX_ROUTINE_ARGUMENTS],
            argument_names: [SqlName::EMPTY; MAX_ROUTINE_ARGUMENTS],
            defaulted: [false; MAX_ROUTINE_ARGUMENTS],
            count: arguments.len(),
            variadic_element: signature.variadic_element,
            is_procedure: signature.kind == b'p',
        };
        candidate.argument_oids[..candidate.count].copy_from_slice(arguments);
        for (index, name) in names.iter().enumerate() {
            candidate.argument_names[index] = if name.is_empty() {
                SqlName::EMPTY
            } else {
                SqlName::parse(name).expect("registered PostgreSQL parameter name fits")
            };
        }
        if include_outputs {
            let mut input = 0;
            for (index, mode) in signature.catalog.modes.iter().enumerate() {
                if matches!(*mode, b'i' | b'b' | b'v') {
                    candidate.defaulted[index] = input >= signature.required_arguments;
                    input += 1;
                }
            }
        } else {
            candidate.defaulted[signature.required_arguments..candidate.count].fill(true);
        }
        candidate
    }

    fn selected(&self) -> RoutineCallSelection {
        self.selected
    }
    fn rank(&self) -> usize {
        self.rank
    }
    fn argument_count(&self) -> usize {
        self.count
    }
    fn parameter_name(&self, index: usize) -> &str {
        self.argument_names[index].as_str()
    }
    fn has_default(&self, index: usize) -> bool {
        self.defaulted[index]
    }
    fn variadic_index(&self) -> Option<usize> {
        self.variadic_element
            .and_then(|_| self.count.checked_sub(1))
    }

    fn parameter_oid(&self, index: usize, expanded: bool) -> i32 {
        if expanded && self.variadic_index() == Some(index) {
            self.variadic_element.expect("expanded variadic parameter")
        } else {
            self.argument_oids[index]
        }
    }

    fn mapping(
        &self,
        names: &[Option<&str>],
        count: usize,
        explicit_variadic: bool,
    ) -> Option<[u8; MAX_ROUTINE_ARGUMENTS]> {
        map_parameter_positions(
            self.argument_count(),
            self.variadic_index(),
            |index| Some((self.parameter_name(index), self.has_default(index))),
            names,
            count,
            explicit_variadic,
        )
    }

    fn accepts(
        &self,
        storage: &Storage,
        transaction: u32,
        names: &[Option<&str>],
        actual: &[i32],
        explicit_variadic: bool,
    ) -> bool {
        let Some(mapping) = self.mapping(names, actual.len(), explicit_variadic) else {
            return false;
        };
        let mut binding = PolymorphicBinding::EMPTY;
        for (actual, input) in actual.iter().zip(mapping) {
            let expected = self.parameter_oid(usize::from(input), !explicit_variadic);
            if *actual == expected || expected == oid::ANY {
                continue;
            }
            if let Some(kind) = polymorphic(expected) {
                if *actual != oid::UNKNOWN
                    && binding
                        .bind(kind, base_oid(storage, transaction, *actual))
                        .is_none()
                {
                    return false;
                }
            } else if !storage.routine_implicit_cast(*actual, expected, transaction) {
                return false;
            }
        }
        true
    }

    fn score(
        &self,
        storage: &Storage,
        transaction: u32,
        names: &[Option<&str>],
        actual: &[i32],
        explicit_variadic: bool,
    ) -> (usize, usize, usize) {
        let mapping = self
            .mapping(names, actual.len(), explicit_variadic)
            .expect("admitted input mapping");
        let mut exact = 0;
        let mut preferred = 0;
        let mut declared_exact = true;
        for (actual, input) in actual.iter().zip(mapping) {
            let expected = self.parameter_oid(usize::from(input), !explicit_variadic);
            declared_exact &= *actual != oid::UNKNOWN && *actual == expected;
            let actual = base_oid(storage, transaction, *actual);
            exact += usize::from(actual != oid::UNKNOWN && actual == expected);
            preferred += usize::from(
                actual != oid::UNKNOWN
                    && actual != expected
                    && category(storage, transaction, actual)
                        == category(storage, transaction, expected)
                    && preferred_type(expected),
            );
        }
        (usize::from(declared_exact), exact, preferred)
    }

    fn same_expanded_signature(
        &self,
        other: &Self,
        names: &[Option<&str>],
        count: usize,
        explicit_variadic: bool,
    ) -> bool {
        let Some(left) = self.mapping(names, count, explicit_variadic) else {
            return false;
        };
        let Some(right) = other.mapping(names, count, explicit_variadic) else {
            return false;
        };
        left[..count]
            .iter()
            .zip(&right[..count])
            .all(|(left, right)| {
                self.parameter_oid(usize::from(*left), !explicit_variadic)
                    == other.parameter_oid(usize::from(*right), !explicit_variadic)
            })
    }
}

fn polymorphic(type_oid: i32) -> Option<PolymorphicType> {
    PolymorphicType::from_oid(type_oid)
}

fn base_oid(storage: &Storage, transaction: u32, type_oid: i32) -> i32 {
    storage
        .domain_base_type_for_oid(type_oid, transaction)
        .map_or(type_oid, ColType::oid)
}

fn category(storage: &Storage, transaction: u32, type_oid: i32) -> &'static str {
    if polymorphic(type_oid).is_some() || type_oid == oid::ANY {
        return "P";
    }
    storage
        .routine_result_for_oid(type_oid, transaction)
        .map_or("P", |result| type_category(result.ctype))
}

// PostgreSQL 18.6 pg_type, typispreferred; the reference query and digest are
// recorded with the callable contract extraction evidence.
fn preferred_type(type_oid: i32) -> bool {
    matches!(type_oid, 16 | 25 | 26 | 701 | 869 | 1184 | 1186 | 1562)
}

impl Storage {
    fn callable_candidates<'a>(
        &'a self,
        written: &'a str,
        transaction: u32,
        include_outputs: bool,
    ) -> impl Iterator<Item = Candidate> + 'a {
        let (qualifier, name) = written
            .split_once('.')
            .map_or((None, written), |(schema, name)| (Some(schema), name));
        let mut next_slot = 0;
        let stored = std::iter::from_fn(move || {
            loop {
                let selected = {
                    let catalog = self.routines.lock().expect("routine catalog lock poisoned");
                    let mut selected = None;
                    while let Some(routine) = catalog.get(next_slot) {
                        let slot = next_slot;
                        next_slot += 1;
                        if routine.database != current_database()
                            || !routine.visible_to(transaction)
                            || routine.name_for(transaction).as_str() != name
                        {
                            continue;
                        }
                        selected = Some((
                            slot,
                            routine.schema_for(transaction),
                            RoutineInputSignature::from_definition(routine, transaction),
                        ));
                        break;
                    }
                    selected
                };
                let (slot, schema, signature) = selected?;
                let rank = if let Some(qualifier) = qualifier {
                    if qualifier != schema.as_str() {
                        continue;
                    }
                    0
                } else {
                    let Some(rank) = self.path_namespace_rank(schema.as_str(), transaction) else {
                        continue;
                    };
                    rank
                };
                if let Some(candidate) =
                    Candidate::stored(self, transaction, slot, rank, &signature, include_outputs)
                {
                    return Some(candidate);
                }
            }
        });
        let builtin = builtin_callable_signatures(name).filter_map(move |signature| {
            // A catalog-owned SQL implementation retains its executable body.
            // The virtual declaration is the same identity, not an overload.
            if self
                .routine_slot_by_oid(signature.oid, transaction)
                .is_some()
            {
                return None;
            }
            let rank = if let Some(qualifier) = qualifier {
                if qualifier != signature.schema {
                    return None;
                }
                0
            } else {
                self.path_namespace_rank(signature.schema, transaction)?
            };
            Some(Candidate::builtin(rank, signature, include_outputs))
        });
        stored.chain(builtin)
    }

    pub(crate) fn builtin_call_mapping(
        routine_oid: i32,
        argument_names: &[Option<&str>],
        written_count: usize,
        explicit_variadic: bool,
    ) -> Option<[u8; MAX_ROUTINE_ARGUMENTS]> {
        let signature = crate::sql::catalog::builtin_callable_by_oid(routine_oid)?;
        Candidate::builtin(0, signature, false).mapping(
            argument_names,
            written_count,
            explicit_variadic,
        )
    }

    pub(crate) fn builtin_call_input_oids(
        &self,
        routine_oid: i32,
        argument_names: &[Option<&str>],
        actual_oids: &[i32],
        explicit_variadic: bool,
        txid: u32,
    ) -> Option<[i32; MAX_ROUTINE_ARGUMENTS]> {
        let signature = crate::sql::catalog::builtin_callable_by_oid(routine_oid)?;
        let candidate = Candidate::builtin(0, signature, false);
        if !candidate.accepts(self, txid, argument_names, actual_oids, explicit_variadic) {
            return None;
        }
        let mapping = candidate.mapping(argument_names, actual_oids.len(), explicit_variadic)?;
        let mut declared = [oid::UNKNOWN; MAX_ROUTINE_ARGUMENTS];
        for (input, mapped) in declared.iter_mut().zip(mapping).take(actual_oids.len()) {
            *input = candidate.parameter_oid(usize::from(mapped), !explicit_variadic);
        }
        self.bind_builtin_input_types(&declared[..actual_oids.len()], actual_oids, txid)
    }

    /// The types a selected built-in gives its untyped parameter inputs. An
    /// input declared `"any"` stays `"any"`: it does not determine a type.
    pub(crate) fn builtin_call_parameter_oids(
        &self,
        routine_oid: i32,
        argument_names: &[Option<&str>],
        actual_oids: &[i32],
        explicit_variadic: bool,
        txid: u32,
    ) -> Option<[i32; MAX_ROUTINE_ARGUMENTS]> {
        let mut expected = self.builtin_call_input_oids(
            routine_oid,
            argument_names,
            actual_oids,
            explicit_variadic,
            txid,
        )?;
        let signature = crate::sql::catalog::builtin_callable_by_oid(routine_oid)?;
        let candidate = Candidate::builtin(0, signature, false);
        let mapping = candidate.mapping(argument_names, actual_oids.len(), explicit_variadic)?;
        for ((output, mapped), &actual) in expected.iter_mut().zip(mapping).zip(actual_oids) {
            if actual == oid::UNKNOWN
                && candidate.parameter_oid(usize::from(mapped), !explicit_variadic) == oid::ANY
            {
                *output = oid::ANY;
            }
        }
        Some(expected)
    }

    fn bind_builtin_input_types(
        &self,
        declared: &[i32],
        actual: &[i32],
        txid: u32,
    ) -> Option<[i32; MAX_ROUTINE_ARGUMENTS]> {
        if declared.len() != actual.len() || actual.len() > MAX_ROUTINE_ARGUMENTS {
            return None;
        }
        let mut binding = PolymorphicBinding::EMPTY;
        for (&declared, &actual) in declared.iter().zip(actual) {
            if let Some(kind) = polymorphic(declared)
                && actual != oid::UNKNOWN
            {
                binding.bind(kind, base_oid(self, txid, actual))?;
            }
        }
        let mut expected = [oid::UNKNOWN; MAX_ROUTINE_ARGUMENTS];
        for ((output, &declared), &actual) in expected.iter_mut().zip(declared).zip(actual) {
            *output = if declared == oid::ANY {
                actual
            } else if let Some(kind) = polymorphic(declared) {
                binding.concrete_oid(kind)?
            } else {
                declared
            };
        }
        Some(expected)
    }

    pub(crate) fn builtin_operator_input_oids(
        &self,
        name: &str,
        left: Option<i32>,
        right: Option<i32>,
        txid: u32,
    ) -> Option<(Option<i32>, Option<i32>)> {
        let (expected_left, expected_right) =
            crate::sql::catalog::builtin_operator_input_oids(name, left, right)?;
        let mut declared = [oid::UNKNOWN; 2];
        let mut actual = [oid::UNKNOWN; 2];
        let mut positions = [None; 2];
        let mut count = 0;
        for (position, (source, target)) in positions
            .iter_mut()
            .zip([(left, expected_left), (right, expected_right)])
        {
            match (source, target) {
                (Some(source), Some(target)) => {
                    declared[count] = target;
                    actual[count] = source;
                    *position = Some(count);
                    count += 1;
                }
                (None, None) => {}
                _ => return None,
            }
        }
        let bound = self.bind_builtin_input_types(&declared[..count], &actual[..count], txid)?;
        Some((
            positions[0].map(|index| bound[index]),
            positions[1].map(|index| bound[index]),
        ))
    }

    pub(crate) fn builtin_call_result_oid(
        &self,
        routine_oid: i32,
        argument_names: &[Option<&str>],
        argument_oids: &[i32],
        explicit_variadic: bool,
        transaction: u32,
    ) -> Option<i32> {
        let signature = crate::sql::catalog::builtin_callable_by_oid(routine_oid)?;
        let Some(result_kind) = polymorphic(signature.result_oid) else {
            return Some(signature.result_oid);
        };
        let candidate = Candidate::builtin(0, signature, false);
        let mapping = candidate.mapping(argument_names, argument_oids.len(), explicit_variadic)?;
        let mut binding = PolymorphicBinding::EMPTY;
        for (actual, input) in argument_oids.iter().zip(mapping) {
            let expected = candidate.parameter_oid(usize::from(input), !explicit_variadic);
            if let Some(kind) = polymorphic(expected)
                && *actual != oid::UNKNOWN
            {
                binding.bind(kind, base_oid(self, transaction, *actual))?;
            }
        }
        binding.concrete_oid(result_kind)
    }

    pub(crate) fn procedure_call_parameter_oids(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        explicit_variadic: bool,
        actual_oids: &[i32],
        txid: u32,
    ) -> Option<[i32; MAX_ROUTINE_ARGUMENTS]> {
        let RoutineCallSelection::Stored(slot) = self.select_routine_call(
            name,
            argument_names,
            actual_oids,
            explicit_variadic,
            txid,
            RoutineCallKind::Procedure,
        ) else {
            return None;
        };
        let routine = self.routine_for(slot, txid);
        let signature = RoutineInputSignature::from_definition(&routine, txid);
        let candidate = Candidate::stored(self, txid, slot, 0, &signature, true)?;
        let mapping = candidate.mapping(argument_names, actual_oids.len(), explicit_variadic)?;
        let mut expected = [oid::UNKNOWN; MAX_ROUTINE_ARGUMENTS];
        for (index, actual) in actual_oids.iter().enumerate() {
            let declared = candidate.parameter_oid(usize::from(mapping[index]), !explicit_variadic);
            expected[index] = if polymorphic(declared).is_some() {
                *actual
            } else {
                declared
            };
        }
        Some(expected)
    }

    pub(crate) fn select_routine_call(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        argument_oids: &[i32],
        explicit_variadic: bool,
        transaction: u32,
        kind: RoutineCallKind,
    ) -> RoutineCallSelection {
        if argument_oids.len() > MAX_ROUTINE_ARGUMENTS
            || (!argument_names.is_empty() && argument_names.len() != argument_oids.len())
        {
            return RoutineCallSelection::Absent;
        }
        let candidates = || {
            self.callable_candidates(
                name,
                transaction,
                matches!(kind, RoutineCallKind::Procedure),
            )
        };
        let admitted = |candidate: &Candidate| {
            candidate.accepts(
                self,
                transaction,
                argument_names,
                argument_oids,
                explicit_variadic,
            )
        };
        let visible = |candidate: &Candidate| {
            !candidates().any(|other| {
                admitted(&other)
                    && other.selected() != candidate.selected()
                    && (other.rank() < candidate.rank()
                        || other.rank() == candidate.rank()
                            && !explicit_variadic
                            && other.variadic_index().is_none()
                            && candidate.variadic_index().is_some())
                    && candidate.same_expanded_signature(
                        &other,
                        argument_names,
                        argument_oids.len(),
                        explicit_variadic,
                    )
            })
        };
        let best = candidates()
            .filter(|candidate| admitted(candidate) && visible(candidate))
            .map(|candidate| {
                candidate.score(
                    self,
                    transaction,
                    argument_names,
                    argument_oids,
                    explicit_variadic,
                )
            })
            .max();
        let Some(best) = best else {
            return RoutineCallSelection::Absent;
        };
        let competitive = |candidate: &Candidate| {
            admitted(candidate)
                && visible(candidate)
                && candidate.score(
                    self,
                    transaction,
                    argument_names,
                    argument_oids,
                    explicit_variadic,
                ) == best
        };
        let mut unknown_categories = [None; MAX_ROUTINE_ARGUMENTS];
        let mut unknown_preferred = [false; MAX_ROUTINE_ARGUMENTS];
        for (index, actual) in argument_oids.iter().enumerate() {
            if *actual != oid::UNKNOWN {
                continue;
            }
            let mut selected = None;
            let mut mixed = false;
            let mut string = false;
            for candidate in candidates().filter(|candidate| competitive(candidate)) {
                let mapping = candidate
                    .mapping(argument_names, argument_oids.len(), explicit_variadic)
                    .expect("admitted input mapping");
                let expected =
                    candidate.parameter_oid(usize::from(mapping[index]), !explicit_variadic);
                let current = category(self, transaction, expected);
                mixed |= selected.is_some_and(|prior| prior != current);
                selected = Some(current);
                string |= current == "S";
            }
            let chosen = if string {
                Some("S")
            } else if !mixed {
                selected
            } else {
                None
            };
            unknown_categories[index] = chosen;
            if let Some(chosen) = chosen {
                unknown_preferred[index] = candidates()
                    .filter(|candidate| competitive(candidate))
                    .any(|candidate| {
                        let mapping = candidate
                            .mapping(argument_names, argument_oids.len(), explicit_variadic)
                            .expect("admitted input mapping");
                        let expected = candidate
                            .parameter_oid(usize::from(mapping[index]), !explicit_variadic);
                        category(self, transaction, expected) == chosen && preferred_type(expected)
                    });
            }
        }
        let mut selected = RoutineCallSelection::Absent;
        let mut selected_procedure = false;
        for candidate in candidates().filter(|candidate| competitive(candidate)) {
            let mapping = candidate
                .mapping(argument_names, argument_oids.len(), explicit_variadic)
                .expect("admitted input mapping");
            if argument_oids.iter().enumerate().any(|(index, actual)| {
                if *actual != oid::UNKNOWN {
                    return false;
                }
                let expected =
                    candidate.parameter_oid(usize::from(mapping[index]), !explicit_variadic);
                unknown_categories[index]
                    .is_some_and(|chosen| category(self, transaction, expected) != chosen)
                    || unknown_preferred[index] && !preferred_type(expected)
            }) {
                continue;
            }
            if selected != RoutineCallSelection::Absent {
                return RoutineCallSelection::Ambiguous;
            }
            selected = candidate.selected();
            selected_procedure = candidate.is_procedure;
        }
        if selected != RoutineCallSelection::Absent
            && matches!(kind, RoutineCallKind::Procedure) != selected_procedure
        {
            RoutineCallSelection::WrongKind {
                is_procedure: selected_procedure,
            }
        } else {
            selected
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_variadic_preserves_fixed_signature_mapping() {
        crate::mem::guard::forbid_alloc(|| {
            let parameters = [("marker", false), ("value", false)];
            let mapping = map_parameter_positions(
                2,
                None,
                |index| parameters.get(index).copied(),
                &[Some("value"), Some("marker")],
                2,
                true,
            )
            .unwrap();
            assert_eq!(&mapping[..2], &[1, 0]);
            assert!(
                map_parameter_positions(
                    2,
                    None,
                    |index| parameters.get(index).copied(),
                    &[],
                    1,
                    true
                )
                .is_none()
            );
            let single = |index| (index == 0).then_some(("value", false));
            assert!(map_parameter_positions(1, Some(0), single, &[], 2, true).is_none());
            assert!(map_parameter_positions(1, Some(0), single, &[], 1, true).is_some());
        });
    }

    #[test]
    fn builtin_signatures_keep_named_defaults_variadic_and_catalog_identity() {
        let interval = crate::sql::catalog::builtin_callable_by_oid(3464).unwrap();
        assert_eq!(interval.name, "make_interval");
        let mapping =
            Storage::builtin_call_mapping(interval.oid, &[Some("secs"), Some("years")], 2, false)
                .unwrap();
        assert_eq!(&mapping[..2], &[6, 0]);
        assert!(interval.defaults.is_some());
        assert!(
            Storage::builtin_call_mapping(interval.oid, &[Some("unknown")], 1, false).is_none()
        );
        assert!(
            Storage::builtin_call_mapping(interval.oid, &[Some("years"), Some("years")], 2, false)
                .is_none()
        );
        let concat = builtin_callable_signatures("concat").next().unwrap();
        assert!(Storage::builtin_call_mapping(concat.oid, &[], 0, false).is_none());
        for name in ["count", "sum", "row_number", "unnest", "_pg_expandarray"] {
            assert!(builtin_callable_signatures(name).next().is_some(), "{name}");
        }
        assert_eq!(
            crate::sql::catalog::builtin_callable_schema("_pg_expandarray"),
            Some("information_schema")
        );
        assert_eq!(
            crate::sql::catalog::builtin_callable_schema("unnest"),
            Some("pg_catalog")
        );
        assert_eq!(
            crate::sql::catalog::builtin_callable_schema("not_a_builtin"),
            None
        );
    }
    #[test]
    fn array_polymorphism_preserves_vector_identity_and_rejects_mixed_containers() {
        crate::mem::guard::forbid_alloc(|| {
            for vector in [ColType::Int2Vector, ColType::OidVector] {
                let mut binding = PolymorphicBinding::EMPTY;
                binding.bind(PolymorphicType::Array, vector.oid()).unwrap();
                assert_eq!(
                    binding.concrete_oid(PolymorphicType::Array),
                    Some(vector.oid())
                );
                let element = if vector == ColType::Int2Vector {
                    oid::INT2
                } else {
                    oid::OID
                };
                assert_eq!(
                    binding.concrete_oid(PolymorphicType::Element),
                    Some(element)
                );
                let ordinary = super::super::array_oid_for_element(element).unwrap();
                assert!(binding.bind(PolymorphicType::Array, ordinary).is_none());
                assert!(
                    super::super::polymorphic_element_oid(PolymorphicType::NonArray, vector.oid())
                        .is_none()
                );
            }
            let mut compatible = PolymorphicBinding::EMPTY;
            compatible
                .bind(PolymorphicType::CompatibleArray, ColType::Int2Vector.oid())
                .unwrap();
            assert_eq!(
                compatible.concrete_oid(PolymorphicType::CompatibleArray),
                Some(crate::sql::types::ArrElem::Int2.array_oid())
            );
        });
    }
}
