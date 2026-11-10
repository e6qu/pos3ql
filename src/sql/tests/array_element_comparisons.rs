use super::routine_identity_diagnostics::{NONE, check};

/// PostgreSQL's array comparison operators accept every array type and fail
/// only when they run and need an element operator the type lacks. Expected
/// results come from PostgreSQL 18.6.
#[test]
fn arrays_of_types_without_comparison_fail_when_postgresql_compares_elements() {
    let equality = |name| ('E', "42883", name);
    let refcursor_equality = &[equality(
        "could not identify an equality operator for type refcursor",
    )][..];
    let json_equality = &[equality(
        "could not identify an equality operator for type json",
    )][..];
    check(&[
        (
            "SELECT ARRAY['a']::refcursor[] = ARRAY['a']::refcursor[]",
            refcursor_equality,
        ),
        (
            "SELECT ARRAY['a']::refcursor[] <> ARRAY['a']::refcursor[]",
            refcursor_equality,
        ),
        (
            "SELECT '{}'::refcursor[] = '{}'::refcursor[]",
            refcursor_equality,
        ),
        (
            "SELECT ARRAY['{}']::json[] = ARRAY['{}']::json[]",
            json_equality,
        ),
        (
            "SELECT ARRAY['a']::refcursor[] < ARRAY['a','b']::refcursor[]",
            &[equality(
                "could not identify a comparison function for type refcursor",
            )],
        ),
        (
            "SELECT '{}'::json[] < '{}'::json[]",
            &[equality(
                "could not identify a comparison function for type json",
            )],
        ),
        (
            "SELECT '{}'::cid[] < '{}'::cid[]",
            &[equality(
                "could not identify a comparison function for type cid",
            )],
        ),
        ("SELECT '{}'::cid[] = '{}'::cid[]", NONE),
        // Different shapes are unequal before any element is compared.
        (
            "SELECT ARRAY['a']::refcursor[] = ARRAY['a','b']::refcursor[]",
            NONE,
        ),
        (
            "SELECT ARRAY['a']::refcursor[] IN (ARRAY['a','b']::refcursor[])",
            NONE,
        ),
        ("SELECT NULL::refcursor[] = ARRAY['a']::refcursor[]", NONE),
        ("SELECT ARRAY['a']::refcursor[] IS DISTINCT FROM NULL", NONE),
        (
            "SELECT ARRAY['a']::refcursor[] IS DISTINCT FROM ARRAY['a']::refcursor[]",
            refcursor_equality,
        ),
        (
            "SELECT NULLIF(ARRAY['a']::refcursor[], ARRAY['b']::refcursor[])",
            refcursor_equality,
        ),
        (
            "SELECT CASE ARRAY['a']::refcursor[] WHEN ARRAY['a']::refcursor[] THEN 1 END",
            refcursor_equality,
        ),
        ("SELECT '{}'::json[] @> '{}'::json[]", json_equality),
        (
            "SELECT ARRAY['a']::refcursor[] && '{}'::refcursor[]",
            refcursor_equality,
        ),
        ("SELECT array_position('{}'::refcursor[], 'a')", NONE),
        ("SELECT array_position('{a}'::refcursor[], NULL)", NONE),
        (
            "SELECT array_position('{a}'::refcursor[], 'a')",
            refcursor_equality,
        ),
        ("SELECT array_positions('{a}'::refcursor[], NULL)", NONE),
        (
            "SELECT array_positions('{a}'::refcursor[], 'a')",
            refcursor_equality,
        ),
        ("SELECT array_remove('{}'::refcursor[], 'a')", NONE),
        (
            "SELECT array_remove('{a}'::refcursor[], NULL)",
            refcursor_equality,
        ),
        ("CREATE TABLE cursor_arrays (handles refcursor[])", NONE),
        ("SELECT handles = handles FROM cursor_arrays", NONE),
        (
            "SELECT handles FROM cursor_arrays ORDER BY handles",
            &[equality(
                "could not identify an ordering operator for type refcursor[]",
            )],
        ),
        (
            "SELECT DISTINCT handles FROM cursor_arrays",
            &[equality(
                "could not identify an equality operator for type refcursor[]",
            )],
        ),
        (
            "SELECT ARRAY['a']::refcursor[] = ANY (ARRAY['a']::refcursor[])",
            &[equality("operator does not exist: refcursor[] = refcursor")],
        ),
    ]);
}
