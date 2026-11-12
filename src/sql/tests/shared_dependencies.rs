use super::*;

/// `DROP ROLE` lists the objects that still depend on the role, as
/// PostgreSQL's `DETAIL` does, from the same rows `pg_shdepend` shows.
#[test]
fn drop_role_details_owners_and_privileges() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE holder; CREATE ROLE reader; \
         CREATE SCHEMA hs AUTHORIZATION holder; CREATE TABLE t(a integer, b text); \
         ALTER TABLE t OWNER TO holder; ALTER FUNCTION lower(text) OWNER TO holder; \
         GRANT SELECT ON t TO reader; GRANT UPDATE (b) ON t TO reader; \
         GRANT EXECUTE ON FUNCTION upper(text) TO reader",
    );
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    let output = run_with(&mut engine, &mut budget, "DROP ROLE holder");
    let text = String::from_utf8_lossy(&output);
    assert_eq!(error_sqlstates(&output), ["2BP01"], "{text}");
    assert!(
        text.contains("owner of schema hs\nowner of table t")
            && text.contains("owner of function lower(text)"),
        "{text}"
    );
    let output = run_with(&mut engine, &mut budget, "DROP ROLE reader");
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains(
            "privileges for table t\nprivileges for function upper(text)\n\
             privileges for column b of table t"
        ),
        "{text}"
    );
    let output = run_with(
        &mut engine,
        &mut budget,
        "SELECT count(*) FROM pg_shdepend WHERE refobjid = 'postgres'::regrole; \
         SELECT pg_describe_object(classid, objid, objsubid), deptype FROM pg_shdepend \
          WHERE refobjid = 'reader'::regrole ORDER BY 1, 2",
    );
    assert_eq!(
        data_rows(&output),
        [
            "0",
            "column b of table t|a",
            "function upper(text)|a",
            "table t|a"
        ]
    );
}
