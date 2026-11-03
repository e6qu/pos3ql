use super::*;

#[test]
fn procedural_expressions_resume_nested_calls_without_repeating_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-expression-calls-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "SET application_name TO 'outer-call'; CREATE SEQUENCE procedural_calls; \
             CREATE TABLE procedural_audit(value integer); \
             CREATE FUNCTION procedural_value(input_value integer) RETURNS integer LANGUAGE plpgsql \
               SET application_name TO 'nested-call' AS $$BEGIN \
                 IF current_setting('application_name')<>'nested-call' THEN RAISE EXCEPTION 'configuration scope lost'; END IF; \
                 INSERT INTO procedural_audit VALUES(input_value); PERFORM nextval('procedural_calls'); \
                 RETURN input_value; END$$; \
             CREATE FUNCTION procedural_composed() RETURNS integer LANGUAGE plpgsql AS $$ \
               DECLARE initial integer:=procedural_value(1); result integer; \
               BEGIN result:=procedural_value(2); \
                 IF procedural_value(3)=3 THEN result:=result+procedural_value(4); END IF; \
                 RETURN result+procedural_value(5); END$$; \
             CREATE FUNCTION procedural_caught() RETURNS integer LANGUAGE plpgsql AS $$ \
               DECLARE zero integer:=0; result integer; \
               BEGIN BEGIN result:=procedural_value(6)/zero; \
                 EXCEPTION WHEN division_by_zero THEN result:=procedural_value(7); END; \
                 RETURN result; END$$; \
             CREATE FUNCTION procedural_command() RETURNS text LANGUAGE plpgsql AS $$BEGIN \
               INSERT INTO procedural_audit VALUES(9); PERFORM nextval('procedural_calls'); \
               RETURN 'SELECT $1'; END$$; \
             CREATE FUNCTION procedural_dynamic() RETURNS integer LANGUAGE plpgsql AS $$ \
               DECLARE result integer; BEGIN \
                 EXECUTE procedural_command() INTO result USING procedural_value(8); \
                 RETURN result; END$$", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT procedural_composed(),current_setting('application_name'); \
             SELECT procedural_caught(),current_setting('application_name'); \
             SELECT procedural_dynamic(),current_setting('application_name'); \
             SELECT count(*),sum(value) FROM procedural_audit; SELECT currval('procedural_calls')",
                true
            )),
            ["11|outer-call", "7|outer-call", "8|outer-call", "8|39", "9"]
        );
    }
}
