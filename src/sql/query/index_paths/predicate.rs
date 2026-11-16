//! `predicate_implied_by` of `predtest.c` over the condition forms the
//! model reads: whether some conditions prove another, as PostgreSQL
//! proves an index predicate from a query's conditions or drops a filter
//! its index conditions imply.

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::ast::Expr;
use crate::sql::eval::SqlError;
use crate::sql::types::Datum;

use super::Comparison;
use super::clauses::{Clause, Restriction};

/// `MAX_SAOP_ARRAY_SIZE`: a longer array is no AND or OR of its elements.
const MAX_ARRAY: usize = 100;

/// A condition as the proof reads it.
#[derive(Clone, Copy)]
pub(crate) enum Prop<'a> {
    /// `column comparison constant`.
    Compare {
        column: usize,
        comparison: Comparison,
        constant: Datum<'a>,
        source: &'a Expr<'a>,
    },
    NullTest {
        column: usize,
        is_null: bool,
    },
    /// `column comparison ANY (elements)`, or `ALL`.
    Array {
        column: usize,
        comparison: Comparison,
        any: bool,
        elements: &'a [Datum<'a>],
        source: &'a Expr<'a>,
    },
    /// A condition proved only by an equal one.
    Opaque(&'a Expr<'a>),
    And(&'a [Prop<'a>]),
    Or(&'a [Prop<'a>]),
}

impl<'a> Prop<'a> {
    /// The condition `restriction` holds.
    pub(super) fn of(restriction: &Restriction<'a>, arena: &'a Arena) -> Result<Self, SqlError> {
        Ok(match restriction.clause {
            Clause::Compare {
                column,
                comparison,
                constant: Some(constant),
                pattern: false,
                ..
            } => Self::Compare {
                column,
                comparison,
                constant,
                source: restriction.source,
            },
            Clause::NullTest {
                column, is_null, ..
            } => Self::NullTest { column, is_null },
            Clause::Array {
                column,
                comparison,
                any,
                elements: Some(elements),
                ..
            } => Self::Array {
                column,
                comparison,
                any,
                elements,
                source: restriction.source,
            },
            Clause::Or(arms) => {
                let props = arena
                    .alloc_slice_with(arms.len(), |_| Self::Opaque(restriction.source))
                    .map_err(|_| crate::sql::eval::arena_full())?;
                for (prop, arm) in props.iter_mut().zip(arms.iter()) {
                    *prop = Self::all(arm, arena)?;
                }
                Self::Or(props)
            }
            _ => Self::Opaque(restriction.source),
        })
    }

    /// The AND of `restrictions`.
    pub(super) fn all(
        restrictions: &[Restriction<'a>],
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        if restrictions.len() == 1 {
            return Self::of(&restrictions[0], arena);
        }
        let props = arena
            .alloc_slice_with(restrictions.len(), |_| Self::And(&[]))
            .map_err(|_| crate::sql::eval::arena_full())?;
        for (prop, restriction) in props.iter_mut().zip(restrictions) {
            *prop = Self::of(restriction, arena)?;
        }
        Ok(Self::And(props))
    }

    /// `predicate_classify`: the parts of an AND or an OR, or `None` for an
    /// atom.
    fn parts(&self, arena: &'a Arena) -> Result<Option<(bool, &'a [Prop<'a>])>, SqlError> {
        Ok(match *self {
            Self::And(parts) => Some((true, parts)),
            Self::Or(parts) => Some((false, parts)),
            Self::Array {
                column,
                comparison,
                any,
                elements,
                source,
            } if elements.len() <= MAX_ARRAY => {
                let parts = arena
                    .alloc_slice_with(elements.len(), |position| Self::Compare {
                        column,
                        comparison,
                        constant: elements[position],
                        source,
                    })
                    .map_err(|_| crate::sql::eval::arena_full())?;
                Some((!any, &*parts))
            }
            _ => None,
        })
    }
}

/// Compares two constants of the column.
pub(super) type Compare<'c> =
    dyn Fn(usize, &Datum<'_>, &Datum<'_>) -> Result<Ordering, SqlError> + 'c;

/// `predicate_implied_by_recurse`: whether `clause` proves `predicate`.
pub(super) fn implied_by<'a>(
    predicate: &Prop<'a>,
    clause: &Prop<'a>,
    compare: &Compare<'_>,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let clause_parts = clause.parts(arena)?;
    let predicate_parts = predicate.parts(arena)?;
    match (clause_parts, predicate_parts) {
        // AND => AND: each of B's items; AND => atom: one of A's items.
        (Some((true, clauses)), predicate_class) => {
            if let Some((true, predicates)) = predicate_class {
                for predicate in predicates {
                    if !implied_by(predicate, clause, compare, arena)? {
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            if let Some((false, predicates)) = predicate_class {
                for predicate in predicates {
                    if implied_by(predicate, clause, compare, arena)? {
                        return Ok(true);
                    }
                }
            }
            for clause in clauses {
                if implied_by(predicate, clause, compare, arena)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        // OR => OR: each of A's items implies one of B's; OR => AND or
        // atom: each of A's items implies B.
        (Some((false, clauses)), predicate_class) => {
            for clause in clauses {
                let proved = match predicate_class {
                    Some((false, predicates)) => {
                        let mut any = false;
                        for predicate in predicates {
                            if implied_by(predicate, clause, compare, arena)? {
                                any = true;
                                break;
                            }
                        }
                        any
                    }
                    _ => implied_by(predicate, clause, compare, arena)?,
                };
                if !proved {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (None, Some((true, predicates))) => {
            for predicate in predicates {
                if !implied_by(predicate, clause, compare, arena)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (None, Some((false, predicates))) => {
            for predicate in predicates {
                if implied_by(predicate, clause, compare, arena)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        (None, None) => simple(predicate, clause, compare),
    }
}

/// `RC_implic_table`: the test `predicate constant ? clause constant` that
/// proves `x pred_op const2` from `x clause_op const1`.
fn implication_test(clause: Comparison, predicate: Comparison) -> Option<Comparison> {
    use Comparison::{Eq, Ge, Gt, Le, Lt, Ne};
    let row = match clause {
        Lt => [Some(Ge), Some(Ge), None, None, None, Some(Ge)],
        Le => [Some(Gt), Some(Ge), None, None, None, Some(Gt)],
        Eq => [Some(Gt), Some(Ge), Some(Eq), Some(Le), Some(Lt), Some(Ne)],
        Ge => [None, None, None, Some(Le), Some(Lt), Some(Lt)],
        Gt => [None, None, None, Some(Le), Some(Le), Some(Le)],
        Ne => [None, None, None, None, None, Some(Eq)],
    };
    let column = match predicate {
        Lt => 0,
        Le => 1,
        Eq => 2,
        Ge => 3,
        Gt => 4,
        Ne => 5,
    };
    row[column]
}

/// `predicate_implied_by_simple_clause` of two atoms.
fn simple(
    predicate: &Prop<'_>,
    clause: &Prop<'_>,
    compare: &Compare<'_>,
) -> Result<bool, SqlError> {
    match (*predicate, *clause) {
        (Prop::Opaque(predicate), Prop::Opaque(clause)) => Ok(*predicate == *clause),
        (Prop::Opaque(predicate), Prop::Compare { source, .. } | Prop::Array { source, .. }) => {
            Ok(*predicate == *source)
        }
        (
            Prop::NullTest {
                column: predicate,
                is_null: predicate_null,
            },
            Prop::NullTest { column, is_null },
        ) => Ok(predicate == column && predicate_null == is_null),
        // A clause strict for the column proves it is not NULL.
        (
            Prop::NullTest {
                column: predicate,
                is_null: false,
            },
            Prop::Compare { column, .. },
        ) => Ok(predicate == column),
        (
            Prop::NullTest {
                column: predicate,
                is_null: false,
            },
            Prop::Array {
                column,
                any,
                elements,
                ..
            },
        ) => Ok(predicate == column && (!any || !elements.is_empty())),
        (
            Prop::Compare {
                column: predicate_column,
                comparison: predicate_comparison,
                constant: predicate_constant,
                ..
            },
            Prop::Compare {
                column,
                comparison,
                constant,
                ..
            },
        ) => {
            if predicate_column != column || predicate_constant.is_null() || constant.is_null() {
                return Ok(false);
            }
            let Some(test) = implication_test(comparison, predicate_comparison) else {
                return Ok(false);
            };
            Ok(test.holds(compare(column, &predicate_constant, &constant)?))
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compare(_: usize, left: &Datum<'_>, right: &Datum<'_>) -> Result<Ordering, SqlError> {
        match (left, right) {
            (Datum::Int4(left), Datum::Int4(right)) => Ok(left.cmp(right)),
            _ => unreachable!("the tests compare int4 constants"),
        }
    }

    fn atom(comparison: Comparison, constant: i32) -> Prop<'static> {
        Prop::Compare {
            column: 0,
            comparison,
            constant: Datum::Int4(constant),
            source: &Expr::Null,
        }
    }

    #[test]
    fn btree_comparisons_prove_wider_ranges_and_not_null() {
        let mut budget = crate::mem::Budget::new(1 << 16);
        let arena = Arena::new(&mut budget, "predicate proofs", 1 << 16).expect("arena");
        let proves = |predicate: Prop<'static>, clause: Prop<'static>| {
            implied_by(&predicate, &clause, &compare, &arena).expect("proof")
        };
        // `x = 5` proves `x < 10`, `x <= 5`, `x <> 7` and `x IS NOT NULL`.
        assert!(proves(atom(Comparison::Lt, 10), atom(Comparison::Eq, 5)));
        assert!(proves(atom(Comparison::Le, 5), atom(Comparison::Eq, 5)));
        assert!(proves(atom(Comparison::Ne, 7), atom(Comparison::Eq, 5)));
        assert!(proves(
            Prop::NullTest {
                column: 0,
                is_null: false
            },
            atom(Comparison::Gt, 1)
        ));
        // `x < 10` proves `x < 20` but not `x < 5`.
        assert!(proves(atom(Comparison::Lt, 20), atom(Comparison::Lt, 10)));
        assert!(!proves(atom(Comparison::Lt, 5), atom(Comparison::Lt, 10)));
        // An IN list proves a range holding each element; an OR needs each
        // arm proved.
        static ELEMENTS: [Datum<'static>; 2] = [Datum::Int4(1), Datum::Int4(2)];
        let list = Prop::Array {
            column: 0,
            comparison: Comparison::Eq,
            any: true,
            elements: &ELEMENTS,
            source: &Expr::Null,
        };
        assert!(proves(atom(Comparison::Lt, 10), list));
        assert!(!proves(atom(Comparison::Lt, 2), list));
        // AND => atom: one part proves it.
        let parts = arena
            .alloc_slice_copy(&[atom(Comparison::Gt, 50), atom(Comparison::Eq, 3)])
            .expect("parts");
        assert!(
            implied_by(
                &atom(Comparison::Le, 3),
                &Prop::And(parts),
                &compare,
                &arena
            )
            .expect("proof")
        );
    }
}
