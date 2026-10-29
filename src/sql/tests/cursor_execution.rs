use super::*;

#[test]
fn cursor_declaration_rejects_conflicting_options_and_write_effects() {
    let config = test_config("cursor-declaration-errors");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut errors = Vec::new();
    let mut rows = Vec::new();
    for sql in include_str!("../../../tests/external/differential_exact/35_cursor_declarations.sql")
        .split(';')
        .filter(|sql| !sql.trim().is_empty())
    {
        let output = session.execute(&mut engine, sql, true);
        errors.extend(error_sqlstates(&output).into_iter().map(str::to_owned));
        rows.extend(data_rows(&output));
    }
    assert_eq!(
        errors,
        [
            "42P11", "42P11", "42P11", "42P11", "0A000", "0A000", "42P11", "0A000", "0A000",
            "0A000", "0A000"
        ]
    );
    assert_eq!(rows, ["2|3", "1|f"]);
}
