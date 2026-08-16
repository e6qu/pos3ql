//! Gate: the no-op guard (`tools/check-noops.sh`) must pass. This makes
//! `cargo test` fail on any new silent accept-and-ignore of SQL/protocol
//! semantics, so a gap gets implemented or rejected loudly — never quietly
//! skipped.

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

#[test]
fn jdbc_differential_never_compares_missing_driver_failures() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/external/ci_drivers.sh");
    let source = std::fs::read_to_string(path).expect("readable driver harness");
    assert!(
        source.contains("curl --fail --silent --show-error --location --retry 2"),
        "the mandatory JDBC artifact fetch must fail loudly after bounded retries"
    );
    assert!(
        source.contains("jar tf \"$JAR\" >/dev/null"),
        "the JDBC artifact must be verified before a transcript is compared"
    );
    assert!(
        source.contains("jdbc (driver artifact unavailable or invalid)"),
        "an unavailable JDBC artifact must fail the driver job, not emulate a protocol diff"
    );
}
