//! Gate: the no-op guard (`tools/check-noops.sh`) must pass. This makes
//! `cargo test` fail on any new silent accept-and-ignore of SQL/protocol
//! semantics, so a gap gets implemented or rejected loudly — never quietly
//! skipped. See BUGS.md B-019/B-020/B-021 for the tracked burn-down.

use std::process::Command;

#[test]
fn no_untracked_noops() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/check-noops.sh");
    let output = Command::new("zsh")
        .arg(script)
        .output()
        .expect("run the no-op guard (needs zsh on PATH)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "no-op guard failed — a new untracked no-op was introduced:\n{stdout}\n{stderr}"
    );
}
