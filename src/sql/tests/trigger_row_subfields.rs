use super::*;

#[test]
fn trigger_functions_assign_sub_fields_of_new_and_old_composite_columns() {
    let config = test_config("trigger-row-subfields");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for statement in [
        "CREATE TYPE trs_pair AS (a int, b int)",
        "CREATE TYPE trs_outer AS (p trs_pair, c int)",
        "CREATE TABLE trs (id int, r trs_pair, o trs_outer)",
        "CREATE FUNCTION trs_f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         NEW.r.a := 10; NEW.o.p.b := 20; NEW.o.c := (NEW.o).c + 1; RETURN NEW; END $$",
        "CREATE TRIGGER trs_t BEFORE INSERT ON trs FOR EACH ROW EXECUTE FUNCTION trs_f()",
        "INSERT INTO trs VALUES (1, ROW(1, 2), ROW(ROW(3, 4), 5))",
    ] {
        session.success(&mut engine, statement, true);
    }
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT r::text, o::text FROM trs", true)),
        ["(10,2)|(\"(3,20)\",6)"]
    );
    for (body, sqlstate) in [("NEW.r.zz := 1", "42703"), ("NEW.id.x := 1", "42804")] {
        session.success(&mut engine, "DROP TRIGGER trs_t ON trs", true);
        session.success(
            &mut engine,
            &format!(
                "CREATE OR REPLACE FUNCTION trs_f() RETURNS trigger LANGUAGE plpgsql AS \
                 $$ BEGIN {body}; RETURN NEW; END $$"
            ),
            true,
        );
        session.success(
            &mut engine,
            "CREATE TRIGGER trs_t BEFORE INSERT ON trs FOR EACH ROW EXECUTE FUNCTION trs_f()",
            true,
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "INSERT INTO trs VALUES (2, ROW(1, 2), ROW(ROW(3, 4), 5))",
                true
            )),
            [sqlstate],
            "{body}"
        );
    }
}
