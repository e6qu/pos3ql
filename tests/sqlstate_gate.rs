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
        for (number, line) in source.lines().enumerate() {
            let inline_err = line.contains("sql_err!(\"");
            let inline_field = line.contains("sqlstate: \"");
            // A code alone on its line: rustfmt's multi-line sql_err! layout,
            // which the two substring checks above cannot see.
            let bare_code = {
                let t = line.trim();
                t.len() == 8
                    && t.starts_with('"')
                    && t.ends_with("\",")
                    && t[1..6].bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
            };
            if inline_err || inline_field || bare_code {
                offenders.push(format!("{}:{}: {}", path.display(), number + 1, line.trim()));
            }
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
