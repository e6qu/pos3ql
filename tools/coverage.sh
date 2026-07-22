#!/bin/sh
# Line/region coverage over BOTH test layers, which is the only number that
# means anything here: `cargo test` exercises the crate in-process, while the
# differential corpora, the sqllogictest blocks and the psql golden suite drive
# the *server binary* as a subprocess. Measuring only the first says 56% and
# reports the wire protocol at 6%, which is an artefact of what is instrumented
# rather than of what is tested.
#
# `cargo llvm-cov show-env` exports the RUSTFLAGS and LLVM_PROFILE_FILE that
# make an ordinary `cargo build` emit an instrumented binary; the external
# harnesses then run that binary through POS3QL_BIN and their profiles land in
# the same directory, so one report covers everything.
#
# Ratchet, like tools/check-dups.sh: MIN may be raised as coverage improves and
# is never lowered without a reason.
set -e
here=$(dirname "$0")
cd "$here/.."
MIN=${COVERAGE_MIN:-75}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

command -v cargo-llvm-cov >/dev/null 2>&1 || {
    echo "FAIL: cargo-llvm-cov is not installed (cargo install cargo-llvm-cov)"
    exit 1
}

eval "$(cargo llvm-cov show-env --sh)"
# `cargo llvm-cov clean` shells out to `cargo clean`, which refuses to touch a
# target/ without a CACHEDIR.TAG; drop the raw profiles directly instead.
find target -name '*.profraw' -delete 2>/dev/null || true

# Everything is measured in the release profile so that the unit tests and the
# server binary the external suites drive produce profiles against the same
# objects; mixing profiles makes llvm-cov fail to find one of them.
echo "=== in-process tests ==="
cargo test --lib --release 2>&1 | grep -E '^test result' | tail -1

echo "=== building the instrumented server ==="
# Cargo does not always re-fingerprint on RUSTC_WRAPPER alone, so an existing
# uninstrumented binary can survive this build and silently contribute no
# profile at all. Touching the crate root forces the rebuild.
touch src/lib.rs
cargo build --release 2>&1 | tail -1
BIN="$PWD/target/release/pos3ql"

if [ -n "$POS3QL_VENV" ] && [ -x "/opt/homebrew/opt/postgresql@18/bin/pg_ctl" ]; then
    echo "=== differential suites (server binary) ==="
    # A pipe would mask the suite's exit status, and a suite that aborts (a
    # stale server on the port, say) produces no profile at all -- which shows
    # up as a plausible-looking but far too low coverage figure rather than as
    # a failure. Fail loudly instead.
    for suite in differential run; do
        if ! POS3QL_BIN="$BIN" zsh "tests/external/$suite.sh" > "$TMP/$suite.log" 2>&1; then
            tail -4 "$TMP/$suite.log"
            echo "FAIL: tests/external/$suite.sh did not pass; coverage would understate"
            exit 1
        fi
        tail -2 "$TMP/$suite.log"
    done
else
    echo "=== differential suites SKIPPED (needs POS3QL_VENV and PostgreSQL 18) ==="
    echo "    the reported figure will understate coverage substantially"
fi

echo "=== combined report ==="
cargo llvm-cov report --release --summary-only 2>&1 | tail -3
PCT=$(cargo llvm-cov report --release --summary-only 2>/dev/null | awk '/^TOTAL/ {gsub("%","",$10); print int($10)}')
echo "line coverage: ${PCT}%  (floor ${MIN}%)"
[ "$PCT" -ge "$MIN" ] || { echo "FAIL: line coverage ${PCT}% is below the ${MIN}% floor"; exit 1; }
echo "OK"
