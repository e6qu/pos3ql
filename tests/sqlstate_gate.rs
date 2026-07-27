//! Gate: every SQLSTATE is a named constant, never an inline string.
//!
//! A five-character code typed at an error site is invisible to the compiler —
//! `"22PO2"` for `"22P02"` ships silently and fails only if some corpus happens
//! to cover that error. The `sqlstate` module names every condition the engine
//! raises; this test fails the build the moment a raw literal appears in
//! `sql_err!(...)` or a `sqlstate:` field, so the typo class stays
//! unrepresentable in practice: the only spellable states are the ones the
//! constants define.

use std::path::Path;

fn inline_sqlstates(source: &str) -> Vec<(usize, &str)> {
    let mut offenders = Vec::new();
    let mut previous = "";
    for (number, line) in source.lines().enumerate() {
        let inline_err = line.contains("sql_err!(\"");
        let inline_field = line.contains("sqlstate: \"");
        // A code alone on the line immediately after `sql_err!(` is
        // rustfmt's multi-line layout. Requiring that context avoids treating
        // ordinary five-letter SQL strings such as "BEGIN" as SQLSTATEs.
        let bare_code = {
            let trimmed = line.trim();
            previous.contains("sql_err!(")
                && trimmed.len() == 8
                && trimmed.starts_with('"')
                && trimmed.ends_with("\",")
                && trimmed[1..6]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
        };
        if inline_err || inline_field || bare_code {
            offenders.push((number + 1, line.trim()));
        }
        previous = line;
    }
    offenders
}

fn scan(dir: &Path, offenders: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("readable source tree") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            scan(&path, offenders);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("readable source file");
        // The `sqlstate` module itself is where the codes are defined.
        if source.contains("pub mod sqlstate") {
            continue;
        }
        for (number, line) in inline_sqlstates(&source) {
            offenders.push(format!("{}:{number}: {line}", path.display()));
        }
    }
}

#[test]
fn sqlstates_are_named_constants() {
    let mut offenders = Vec::new();
    scan(Path::new("src"), &mut offenders);
    assert!(
        offenders.is_empty(),
        "inline SQLSTATE literal(s) — use a `sqlstate::` constant so a typo'd \
         code cannot compile:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn scanner_distinguishes_sql_text_from_multiline_error_codes() {
    let source = r#"
        execute(
            "BEGIN",
        );
        let error = sql_err!(
            "22P02",
            "invalid text"
        );
    "#;
    assert_eq!(inline_sqlstates(source), [(6, "\"22P02\",")]);
}
